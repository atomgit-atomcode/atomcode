// crates/atomcode-tuix/src/event_loop/commands.rs
//
// Slash-command dispatcher. Everything the user can invoke by typing
// `/name` lives here — built-in info commands, modal openers, the cd
// helper, and the blocking OAuth flow that suspends the reader + renderer.
//
// ─── bot review response ledger (feat/save-export-markdown, PR #562) ───
// 每条 bot 审查意见均在代码层响应:
//   • Low (07-01) resolve_save Ok 返回路径未 canonicalize,与 doc 不符 → 661fdd9 已改为 canonicalize 后返回,doc 一致
//   • Low (07-03) render_save_markdown 第 4477 行 `_ => continue` 不可达死代码 → 本 commit 改为 unreachable!()
// bot 已在 07-01 22:45 给过「✅ 未发现问题」总结,本轮按其再审建议继续优化。
// 我们愿意根据再审意见继续优化。
//
// New commands should be:
//   1. Registered in `CommandRegistry::builtin` (crates/.../commands.rs)
//   2. Added as an arm in `execute_slash_command` below
//   3. Any long handler factored to a private helper in this file
//
// Modals open by pushing `Some(Box::new(...))` into `active_modal` — the
// handler arms for `/model`, `/resume`, `/provider` show the pattern.

use std::path::{Path, PathBuf};

use super::{
    apply_persisted_config, bg_runtime, deactivate_runtime_provider_after_logout,
    provider_transition_pending, reload_persisted_config, request_context_stats_render,
    save_and_reload, save_language_and_reload, LoopCtx, PersistedConfigReload,
};
use crate::custom_commands::ArgsRequirement;
use crate::i18n::{t, Msg};
use crate::modals::usage::{UsageData, UsageModal};
use crate::modals::{
    ConfigPanel, DiffViewer, DirPicker, FileViewer, LanguagePicker, Modal, ModelPicker, ProxyPicker,
};
use crate::render::{Renderer, UiLine};
use crate::session::{Session, SessionId};
use crate::state::{AgentMode, UiState};
use anyhow::Result;
use atomcode_capabilities::memory::MemoryStore;
use atomcode_config::config::Config;

use crate::markdown::{fence_start, is_closing_fence};

/// Maximum manual MRU project dirs we keep in memory + persist to disk.
///
/// This is only an ordering hint for `/cd`; the picker also includes every
/// valid working directory found in the native session catalog.
const MAX_RECENT_DIRS: usize = 5;

fn foreground_state_from_ui(state: &UiState) -> bg_runtime::RuntimeState {
    if matches!(
        state.phase,
        crate::state::UiPhase::Streaming
            | crate::state::UiPhase::Approval
            | crate::state::UiPhase::UserInput
            | crate::state::UiPhase::RoundCap
    ) {
        bg_runtime::RuntimeState::Running
    } else {
        bg_runtime::RuntimeState::Idle
    }
}

/// `/rewind`: open the checkpoint picker — the exact flow the double-Esc
/// gesture triggers. Kicks off an async catalog refresh; the runtime replies
/// with `RewindCatalogRefreshed`, which the main loop turns into the Rewind
/// modal (or a "no rewind points" notice) via `install_pending_rewind_modal`.
/// Idle-only: rewinding mutates conversation history, so it must not race a
/// running turn (mirrors the double-Esc gate and `dispatch_undo`). Workspace/code
/// Rewind is disabled in v5.0.5.
pub(super) fn dispatch_rewind(state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
    if state.phase != crate::state::UiPhase::Idle {
        renderer.render(UiLine::CommandOutput(t(Msg::CmdRewindBusy).into_owned()));
        renderer.flush();
        return;
    }
    if let Err(error) = ctx
        .runtime
        .refresh_rewind_catalog(ctx.foreground_runtime_id, ctx.runtime_event_tx.clone())
    {
        renderer.render(UiLine::Error(format!("{}: {error}", t(Msg::CmdRewindUnavailable))));
        renderer.flush();
    }
}

/// Translate the compact TUI `/review` syntax into the explicit schema accepted by the
/// `code_review` tool. Keeping this pure makes command semantics testable and avoids the
/// legacy top-level `base` form whose diff included the working tree as an accidental side
/// effect. `/review <base>` now consistently means the committed `<base>..HEAD` range.
fn review_prompt(arg: &str) -> String {
    let scope = arg.trim();
    if scope.is_empty() {
        return "Review my current uncommitted changes: call the `code_review` tool with \
                {\"scope\":{\"kind\":\"working_tree\"}}, then give me a concise summary of \
                its findings."
            .to_string();
    }
    if scope.eq_ignore_ascii_case("staged") {
        return "Review my staged changes: call the `code_review` tool with \
                {\"scope\":{\"kind\":\"staged\"}}, then give me a concise summary of its \
                findings."
            .to_string();
    }
    format!(
        "Review the requested committed range: call the `code_review` tool with \
         {{\"scope\":{{\"kind\":\"range\",\"base\":{base},\"head\":\"HEAD\"}}}}, then give \
         me a concise summary of its findings.",
        base = serde_json::to_string(scope).expect("serializing a string cannot fail")
    )
}

pub(super) fn dispatch_undo(
    arg: &str,
    state: &UiState,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
) {
    if state.phase != crate::state::UiPhase::Idle {
        renderer.render(UiLine::CommandOutput(t(Msg::CmdUndoBusy).into_owned()));
        renderer.flush();
        return;
    }

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
            ctx.runtime
                .undo_to_prompt(nth, ctx.foreground_runtime_id, ctx.runtime_event_tx.clone())
                .ok();
        }
        Err(()) => {
            renderer.render(UiLine::CommandOutput(t(Msg::CmdUndoBadArg).into_owned()));
            renderer.flush();
        }
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
) -> (bg_runtime::RuntimeId, super::RuntimeEndpoint, Session) {
    let runtime_id = ctx.bg_manager.allocate_runtime_id();
    // Spawn through the injected CodingRuntime factory. It reads the CURRENT
    // config/working_dir, keeping
    // /model /provider /cd honoured.
    let spawned = (ctx.runtime_spawn_override)(&ctx.config, &ctx.working_dir, &session);
    bg_runtime::spawn_event_forwarder(runtime_id, spawned.event_rx, ctx.runtime_event_tx.clone());
    (runtime_id, spawned.endpoint, session)
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
        super::RuntimeEndpoint {
            native: ctx.runtime.clone(),
        },
        ctx.current_session.clone(),
        ctx.working_dir.clone(),
    );
}

fn ensure_bg_foreground_switch_allowed(
    live_binding: bool,
    provider_transition: bool,
    pending_runtime_request: bool,
) -> Result<(), &'static str> {
    if provider_transition {
        Err("/bg cannot switch the foreground while a provider transition is in progress")
    } else if pending_runtime_request {
        Err("/bg cannot switch the foreground while an interactive runtime request is pending")
    } else if live_binding {
        Err("/bg cannot switch the foreground while live sync is attached; run /sync off first")
    } else {
        Ok(())
    }
}

fn apply_resumed_runtime_state(state: &mut UiState, runtime_state: bg_runtime::RuntimeState) {
    state.on_session_replaced();
    if matches!(runtime_state, bg_runtime::RuntimeState::Running) {
        state.on_submit();
    } else {
        state.on_turn_complete();
    }
}

fn schedule_resumed_runtime_replay(
    replay_queue: &mut std::collections::VecDeque<bg_runtime::RuntimeEventPayload>,
    events: Vec<bg_runtime::RuntimeEventPayload>,
) {
    replay_queue.extend(events);
}

fn foreground_turn_replay_events(state: &UiState) -> Vec<bg_runtime::RuntimeEventPayload> {
    if !matches!(
        state.phase,
        crate::state::UiPhase::Streaming
            | crate::state::UiPhase::Approval
            | crate::state::UiPhase::UserInput
            | crate::state::UiPhase::RoundCap
    ) {
        return Vec::new();
    }

    let mut events = Vec::new();
    if let Some(message) = state.last_submitted_message.as_ref() {
        events.push(bg_runtime::RuntimeEventPayload::Ui(
            crate::event_loop::ui_event::UiEvent::UserEcho(message.clone()),
        ));
    }
    if !state.response_finalized && !state.last_assistant_response.is_empty() {
        events.push(bg_runtime::RuntimeEventPayload::Ui(
            crate::event_loop::ui_event::UiEvent::TextDelta(state.last_assistant_response.clone()),
        ));
    }
    events
}

fn finalize_background_submission<E>(
    manager: &mut bg_runtime::BgRuntimeManager,
    slot: usize,
    result: Result<(), E>,
) -> Result<(), E> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            manager
                .drop_slot(slot)
                .expect("the newly appended background slot must still exist");
            Err(error)
        }
    }
}

#[cfg(test)]
mod bg_live_guard_tests {
    use std::path::PathBuf;

    use super::{
        apply_resumed_runtime_state, command_output_should_mirror, detach_live_binding_with,
        ensure_bg_foreground_switch_allowed, finalize_background_submission,
        foreground_turn_replay_events, schedule_resumed_runtime_replay,
    };
    use crate::event_loop::bg_runtime::{BgRuntimeManager, RuntimeEventPayload, RuntimeState};
    use crate::session::Session;
    use crate::state::{UiPhase, UiState};

    #[test]
    fn live_binding_blocks_only_foreground_bg_switches() {
        assert!(ensure_bg_foreground_switch_allowed(true, false, false).is_err());
        assert!(ensure_bg_foreground_switch_allowed(false, false, false).is_ok());
    }

    #[test]
    fn provider_transition_blocks_foreground_owner_switches() {
        assert!(ensure_bg_foreground_switch_allowed(false, true, false).is_err());
        assert!(ensure_bg_foreground_switch_allowed(false, false, false).is_ok());
    }

    #[test]
    fn pending_runtime_request_blocks_foreground_owner_switches() {
        assert!(ensure_bg_foreground_switch_allowed(false, false, true).is_err());
        assert!(ensure_bg_foreground_switch_allowed(false, false, false).is_ok());
    }

    #[test]
    fn streaming_footer_reports_are_desktop_local_even_with_live_sync() {
        assert!(!command_output_should_mirror(
            true,
            UiPhase::Streaming,
            "usage"
        ));
        assert!(!command_output_should_mirror(
            true,
            UiPhase::Streaming,
            "cost"
        ));
        assert!(
            command_output_should_mirror(true, UiPhase::Idle, "cost"),
            "idle /cost keeps its existing command-output mirroring"
        );
        assert!(command_output_should_mirror(
            true,
            UiPhase::Streaming,
            "status"
        ));
    }

    #[test]
    fn running_background_resume_keeps_the_foreground_streaming() {
        let mut state = UiState::with_unicode(true);
        state.on_turn_complete();
        state.footer_command_output = Some("old session cost".into());

        apply_resumed_runtime_state(&mut state, RuntimeState::Running);

        assert_eq!(state.phase, UiPhase::Streaming);
        assert!(
            state.footer_command_output.is_none(),
            "resuming another foreground runtime must drop the old session report"
        );
    }

    #[test]
    fn backgrounding_current_turn_keeps_the_already_rendered_prefix() {
        let mut state = UiState::with_unicode(true);
        state.on_submit();
        state.last_submitted_message = Some("question".into());
        state.last_assistant_response = "partial answer".into();
        state.response_finalized = false;

        let events = foreground_turn_replay_events(&state);

        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEventPayload::Ui(crate::event_loop::ui_event::UiEvent::UserEcho(user)),
                RuntimeEventPayload::Ui(crate::event_loop::ui_event::UiEvent::TextDelta(answer)),
            ] if user == "question" && answer == "partial answer"
        ));
    }

    #[test]
    fn failed_live_unbind_preserves_the_local_guard_binding() {
        let original = atomcode_daemon::live_hub::LiveBinding {
            id: 7,
            generation: 3,
            session_id: "session".into(),
            working_dir: PathBuf::from("/project"),
            provider: "provider".into(),
            provider_fingerprint: "fingerprint".into(),
        };
        let mut binding = Some(original.clone());

        let result = detach_live_binding_with(&mut binding, |_| {
            Err(atomcode_daemon::live_hub::HubError::ActiveTurn)
        });

        assert!(result.is_err());
        assert_eq!(binding, Some(original));
    }

    #[test]
    fn failed_background_submit_removes_the_unstarted_slot() {
        let project = PathBuf::from("/project");
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        let slot = manager
            .push_test_background(Session::default_session(project), RuntimeState::Running)
            .unwrap();

        let result =
            finalize_background_submission(&mut manager, slot, Err::<(), _>("runtime unavailable"));

        assert_eq!(result, Err("runtime unavailable"));
        assert!(manager.backgrounds().is_empty());
    }

    #[test]
    fn resumed_request_is_prioritized_ahead_of_the_shared_runtime_queue() {
        let mut transport_queue = std::collections::VecDeque::from([RuntimeEventPayload::Ui(
            crate::event_loop::ui_event::UiEvent::TurnComplete {
                duration: std::time::Duration::default(),
                total_tokens: 0,
                turn_count: 0,
                tool_call_count: 0,
                stop_reason: crate::event_loop::ui_event::UiTurnStopReason::Natural,
                snapshot: atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
            },
        )]);
        let mut replay_queue = std::collections::VecDeque::new();
        let request = atomcode_coding::RuntimeRequest {
            id: 42,
            kind: atomcode_capabilities::tools::APPROVAL_KIND.into(),
            payload: serde_json::json!({}),
            snapshot: None,
        };

        schedule_resumed_runtime_replay(
            &mut replay_queue,
            vec![RuntimeEventPayload::Native(
                atomcode_coding::CodingRuntimeEvent::Request(request),
            )],
        );

        let queued = replay_queue.pop_front().unwrap();
        assert!(matches!(
            queued,
            RuntimeEventPayload::Native(atomcode_coding::CodingRuntimeEvent::Request(request))
                if request.id == 42
        ));
        assert!(matches!(
            transport_queue.pop_front(),
            Some(RuntimeEventPayload::Ui(
                crate::event_loop::ui_event::UiEvent::TurnComplete { .. }
            ))
        ));
    }
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
        return Some(
            t(Msg::SessionNameTooLong {
                max: MAX_SESSION_NAME_LEN,
            })
            .into_owned(),
        );
    }
    if trimmed.chars().any(char::is_control) {
        return Some(t(Msg::SessionNameControlChars).into_owned());
    }
    None
}

/// Rename a session after validation, persist it, and return old/new names.
pub fn perform_session_rename(
    project_bucket: &str,
    session_id: &SessionId,
    new_name: &str,
) -> Result<(String, String), String> {
    if let Some(err) = validate_session_name(new_name) {
        return Err(err);
    }
    let new_name = new_name.trim().to_string();
    let old_name = atomcode_daemon::legacy_convert::rename_catalog_session_in_project(
        project_bucket,
        session_id.as_str(),
        &new_name,
    )
    .map_err(|e| {
        t(Msg::SessionSaveFailed {
            error: &e.to_string(),
        })
        .into_owned()
    })?;
    Ok((old_name, new_name))
}

/// The active runtime directory identifies the physical session bucket. Persisted
/// metadata is display data here: historical duplicates may carry a stale embedded
/// `working_dir` and must never redirect a mutation to another bucket.
pub(super) fn active_session_project_bucket(working_dir: &std::path::Path) -> String {
    atomcode_capabilities::session::SessionManager::project_hash(working_dir)
}

/// Render the "Instruction files:" status block — the same one shown
/// by `/status`, factored out so `/init` can also display it after
/// writing `.atomcode.md` (so users see the new file appear under
/// PROJECT immediately, rather than trusting the success message).
fn render_context_file_status_block(working_dir: &std::path::Path) -> String {
    use atomcode_config::config::instructions::{InstructionLevel, LayeredInstructions};
    let instructions = LayeredInstructions::load(working_dir);
    let mut out = t(Msg::StatusInstructionFilesHeader).into_owned();
    for line in instructions.status_lines(working_dir) {
        let scope = t(match line.level {
            InstructionLevel::Global => Msg::StatusInstructionScopeGlobal,
            InstructionLevel::Project => Msg::StatusInstructionScopeProject,
            InstructionLevel::User => Msg::StatusInstructionScopeUser,
        });
        let path = line.path.display().to_string();
        if line.found {
            out.push_str(&t(Msg::StatusInstructionPresent {
                path: &path,
                label: line.level.label(),
                scope: &scope,
            }));
        } else {
            out.push_str(&t(Msg::StatusInstructionMissing {
                path: &path,
                label: line.level.label(),
                scope: &scope,
            }));
        }
    }
    out.push('\n');
    out.push_str(&t(Msg::StatusMemoryFilesHeader));
    for (scope_msg, store) in [
        (Msg::StatusMemoryScopeGlobal, MemoryStore::global()),
        (
            Msg::StatusMemoryScopeProject,
            MemoryStore::project(working_dir),
        ),
    ] {
        let scope = t(scope_msg);
        let path = store.path().display().to_string();
        if store.path().is_file() {
            out.push_str(&t(Msg::StatusMemoryPresent {
                path: &path,
                scope: &scope,
            }));
        } else {
            out.push_str(&t(Msg::StatusMemoryMissing {
                path: &path,
                scope: &scope,
            }));
        }
    }
    out
}

/// 将当前 TUI Coding Runtime 绑定到 live hub，供 `/webui` 和 `/sync` 共用。
fn live_provider_selection(config: &Config) -> Result<String, String> {
    let selection = super::resolved_provider_and_model(config).0;
    if selection.is_empty() {
        Err("no model is configured; run /login or /provider first".into())
    } else {
        Ok(selection)
    }
}

pub(crate) fn attach_live_runtime(
    ctx: &mut LoopCtx,
    mode: AgentMode,
    renderer: &mut dyn Renderer,
) -> Result<(), String> {
    let snapshot = ctx.current_session.to_conversation_snapshot();
    // The running TUI resolves `default_model` before the legacy
    // `default_provider`, with a catalog fallback when both raw fields are
    // empty. Reuse that exact selection for the live binding. In particular,
    // first login can leave `default_provider == ""` while the runtime already
    // runs the newly published CodingPlan model.
    let provider_selection = live_provider_selection(&ctx.config)?;
    let provider_fingerprint =
        atomcode_daemon::native_live::provider_fingerprint(&ctx.config, &provider_selection)?;
    let binding = atomcode_daemon::native_live::register_embedded_runtime(
        ctx.current_session.id.to_string(),
        ctx.working_dir.clone(),
        provider_selection,
        provider_fingerprint,
        snapshot,
        std::sync::Arc::new(ctx.runtime.clone()),
    )
    .map_err(|error| format!("共享当前 runtime 失败：{error:?}"))?;
    // The runtime binding owns execution; the process-level mode seeds the first
    // live snapshot before any ModeChanged event exists.
    atomcode_daemon::live_set_mode(mode);
    ctx.live_binding = Some(binding);
    let mut remote_commands = atomcode_daemon::native_live::register_remote_command_sink();
    let runtime_id = ctx.foreground_runtime_id;
    let event_tx = ctx.runtime_event_tx.clone();
    tokio::spawn(async move {
        while let Some(command) = remote_commands.recv().await {
            if event_tx
                .send(super::bg_runtime::RuntimeEvent {
                    runtime_id,
                    event: super::bg_runtime::RuntimeEventPayload::Ui(
                        super::ui_event::UiEvent::RemoteSlashCommand(command),
                    ),
                })
                .is_err()
            {
                break;
            }
        }
    });
    // 取消旧 observation 转发任务，避免多次 /app 连接后转发重复
    if let Some(old_task) = ctx.live_observation_task.take() {
        old_task.abort();
    }
    if let Ok(live_join) = atomcode_daemon::native_live::join() {
        let mut receiver = live_join.receiver;
        let event_tx = ctx.runtime_event_tx.clone();
        let runtime_id = ctx.foreground_runtime_id;
        ctx.live_observation_task = Some(tokio::spawn(async move {
            while let Ok(observation) = receiver.recv().await {
                let Some(event) = project_live_view_event(observation.event) else {
                    continue;
                };
                if event_tx
                    .send(super::bg_runtime::RuntimeEvent {
                        runtime_id,
                        event: super::bg_runtime::RuntimeEventPayload::Ui(event),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
    renderer.render(UiLine::CommandOutput(
        "已共享当前会话（与浏览器实时互通）".to_string(),
    ));
    Ok(())
}

fn project_live_view_event(
    event: atomcode_daemon::live_hub::LiveViewEvent,
) -> Option<super::ui_event::UiEvent> {
    match event {
        atomcode_daemon::live_hub::LiveViewEvent::InputAccepted { input, .. } => {
            // Re-attach `[Image #N]` markers dropped by the text-only echo: a webui
            // submit keeps images separate (`input.images`) with no inline markers.
            let echo = super::echo_text_with_image_markers(input.text, input.images.len());
            Some(super::ui_event::UiEvent::UserEcho(echo))
        }
        atomcode_daemon::live_hub::LiveViewEvent::RequestResolved { request_id, kind } => {
            Some(super::ui_event::UiEvent::SharedRequestResolved { request_id, kind })
        }
        _ => None,
    }
}

fn detach_live_runtime(ctx: &mut LoopCtx) -> Result<bool, String> {
    // 取消 observation 转发任务
    if let Some(task) = ctx.live_observation_task.take() {
        task.abort();
    }
    detach_live_binding_with(&mut ctx.live_binding, |binding| {
        atomcode_daemon::native_live::unregister_embedded_runtime(binding)
    })
}

fn detach_live_binding_with(
    binding: &mut Option<atomcode_daemon::live_hub::LiveBinding>,
    unregister: impl FnOnce(
        &atomcode_daemon::live_hub::LiveBinding,
    ) -> Result<(), atomcode_daemon::live_hub::HubError>,
) -> Result<bool, String> {
    let Some(current) = binding.as_ref() else {
        return Ok(false);
    };
    unregister(current).map_err(|error| format!("停止共享当前 runtime 失败：{error:?}"))?;
    *binding = None;
    Ok(true)
}

/// 捕获 `CommandOutput` / `Error`，同步模式下经 live hub 广播给其他视图。
struct CaptureRenderer<'a> {
    inner: &'a mut dyn Renderer,
    captured: String,
}

impl Renderer for CaptureRenderer<'_> {
    fn render(&mut self, line: UiLine) {
        if let UiLine::CommandOutput(s) | UiLine::Error(s) = &line {
            if !self.captured.is_empty() {
                self.captured.push('\n');
            }
            self.captured.push_str(s);
        }
        self.inner.render(line);
    }
    fn flush(&mut self) {
        self.inner.flush();
    }
    fn shutdown(&mut self) {
        self.inner.shutdown();
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn clear_screen(&mut self) {
        self.inner.clear_screen();
    }
    fn suspend_for_external(&mut self) {
        self.inner.suspend_for_external();
    }
    fn resume_from_external(&mut self) {
        self.inner.resume_from_external();
    }
    fn flush_deferred(&mut self) {
        self.inner.flush_deferred();
    }
    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.inner.on_resize(cols, rows);
    }
}

/// 同步模式下输出**不**镜像到手机的命令：它们的输出是桌面侧的接入引导
/// （二维码、浏览器地址、同步提示），对手机端没有意义甚至是噪音。
const MIRROR_EXCLUDED: &[&str] = &["app", "webui", "sync", "login", "logout"];

fn command_output_should_mirror(
    live_binding: bool,
    phase: crate::state::UiPhase,
    cmd: &str,
) -> bool {
    let local_footer_report = matches!(phase, crate::state::UiPhase::Streaming)
        && matches!(cmd.to_ascii_lowercase().as_str(), "usage" | "cost");
    live_binding
        && !local_footer_report
        && !MIRROR_EXCLUDED.contains(&cmd.to_ascii_lowercase().as_str())
}

/// 提交一条「由斜杠命令合成的用户回合」（如 /skills、/review、/guide、自定义命令展开的
/// 模板）到当前生效的对话引擎。
///
/// 已绑定时经 live hub 投递到同一个 Coding Runtime，否则直接投递本地 runtime。
pub(crate) fn submit_agent_turn(ctx: &LoopCtx, state: &mut UiState, text: String) {
    let submitted = submit_agent_text(ctx, text);
    if submitted {
        state.on_submit();
    }
}

fn submit_agent_text(ctx: &LoopCtx, text: String) -> bool {
    if ctx.live_binding.is_some() {
        atomcode_daemon::native_live::submit(atomcode_coding::UserInput::from(text)).is_ok()
    } else {
        ctx.runtime
            .dispatch(atomcode_coding::DriverCommand::Submit(text.into()))
            .is_ok()
    }
}

/// Fire one iteration of a fixed-interval `/loop`.
///
/// Bumps the round, clears `due`, and re-arms `next_fire_at` (so the next
/// wall-clock deadline is measured from *this* fire, not from when the
/// previous payload eventually finished). Then dispatches the payload:
///
/// - `Prompt` → enqueue a `SendMessage` to the agent. Prompts can't be
///   judged success/failure synchronously (the turn runs async), so they
///   always reset `consecutive_failures` — the round either drives a turn
///   to completion or the user stops the loop.
/// - `Slash`  → run `execute_slash_command` inline; its `Result` decides
///   whether `consecutive_failures` increments (3 in a row → `decide`
///   returns `Stop`).
///
/// Callers must thread `execute_slash_command`'s extra params through so a
/// slash payload can open modals / drive setup side-channels exactly as a
/// typed command would.
pub(crate) fn fire_interval_payload(
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
    setup_pending: &mut bool,
) {
    let payload = match ctx.loop_ctrl.as_mut() {
        Some(c) => {
            c.round += 1;
            c.due = false;
            c.next_fire_at = Some(std::time::Instant::now() + c.interval);
            // `state.loop_round` is 0-based (self-paced feeds core's 0-based round
            // here too); every display site adds +1. `c.round` is 1-based after the
            // bump above, so subtract 1 to keep the interval path from showing one
            // round too high (first fire = "round 1", not "round 2").
            state.loop_round = c.round.saturating_sub(1);
            c.payload.clone()
        }
        None => return,
    };
    match payload {
        crate::event_loop::loop_ctrl::LoopPayload::Prompt(text) => {
            let submitted = if ctx.live_binding.is_some() {
                atomcode_daemon::native_live::submit(text.into()).is_ok()
            } else {
                ctx.runtime
                    .dispatch(atomcode_coding::DriverCommand::Submit(text.into()))
                    .is_ok()
            };
            if submitted {
                state.on_submit();
                if let Some(c) = ctx.loop_ctrl.as_mut() {
                    c.consecutive_failures = 0;
                    c.mark_turn_submitted();
                }
            } else if let Some(c) = ctx.loop_ctrl.as_mut() {
                c.consecutive_failures = c.consecutive_failures.saturating_add(1);
            }
        }
        crate::event_loop::loop_ctrl::LoopPayload::Slash { cmd, arg } => {
            let name = cmd.trim_start_matches('/');
            let res = execute_slash_command(
                name,
                &arg,
                state,
                ctx,
                renderer,
                active_modal,
                setup_pending,
            );
            if let Some(c) = ctx.loop_ctrl.as_mut() {
                if res.is_err() {
                    c.consecutive_failures += 1;
                } else {
                    c.consecutive_failures = 0;
                    if !matches!(state.phase, crate::state::UiPhase::Idle) {
                        c.mark_turn_submitted();
                    }
                }
            }
        }
    }
}

/// Fully stop any active `/loop`: sends `ClearLoop` to halt the core
/// self-paced loop engine AND clears the TUI fixed-interval controller plus
/// all three mirror fields. Idempotent — safe to call when no loop is active.
pub(crate) fn stop_active_loop(state: &mut UiState, ctx: &mut LoopCtx) {
    if ctx.loop_ctrl.is_some() || state.loop_label.is_some() {
        ctx.runtime
            .dispatch(atomcode_coding::DriverCommand::StopLoop)
            .ok();
        ctx.loop_ctrl = None;
        state.loop_label = None;
        state.loop_round = 0;
        state.loop_started_at = None;
    }
}

/// Start a fixed-interval `/loop`: parse the raw payload into a
/// `LoopPayload`, install a fresh `LoopController` on `ctx`, seed the
/// status-bar label/round/start-clock, and fire the first iteration
/// immediately (so `/loop 5m /foo` runs `/foo` now, then every 5m).
///
/// A payload starting with `/` is a slash command (split into cmd + arg on
/// the first whitespace); anything else is a free-text prompt.
pub(crate) fn start_interval_loop(
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    secs: u64,
    payload: String,
    active_modal: &mut Option<Box<dyn Modal>>,
    setup_pending: &mut bool,
) {
    let p = if payload.starts_with('/') {
        let (cmd, arg) = payload
            .split_once(char::is_whitespace)
            .unwrap_or((payload.as_str(), ""));
        crate::event_loop::loop_ctrl::LoopPayload::Slash {
            cmd: cmd.to_string(),
            arg: arg.trim().to_string(),
        }
    } else {
        crate::event_loop::loop_ctrl::LoopPayload::Prompt(payload.clone())
    };
    let mut c = crate::event_loop::loop_ctrl::LoopController::new_interval(secs, p);
    c.next_fire_at = Some(std::time::Instant::now() + c.interval);
    // Honor the same TOML + env resolution as the runtime-owned self-paced
    // loop. In particular, ATOMCODE_LOOP_MAX_ROUNDS=0 is unbounded here too.
    c.max_rounds = atomcode_coding::resolve_loop_max_rounds(
        ctx.config.loop_config.max_rounds,
        std::env::var("ATOMCODE_LOOP_MAX_ROUNDS").ok().as_deref(),
    );
    ctx.loop_ctrl = Some(c);
    state.loop_label = Some(format!("{secs}s · {payload}"));
    state.loop_round = 0;
    state.loop_started_at = Some(std::time::Instant::now());
    fire_interval_payload(state, ctx, renderer, active_modal, setup_pending);
}

pub(super) fn execute_slash_command(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
    setup_pending: &mut bool,
) -> Result<()> {
    // Streaming `/usage` and `/cost` are desktop-local footer panels, not
    // conversation output. Do not broadcast them to phone/WebUI merely because
    // live sync is attached. A command initiated by the remote client still
    // uses `run_remote_command` and gets its explicitly requested response.
    let mirror = command_output_should_mirror(ctx.live_binding.is_some(), state.phase, cmd);
    if !mirror {
        return execute_slash_command_impl(
            cmd,
            arg,
            state,
            ctx,
            renderer,
            active_modal,
            setup_pending,
        );
    };
    let mut cap = CaptureRenderer {
        inner: renderer,
        captured: String::new(),
    };
    let result =
        execute_slash_command_impl(cmd, arg, state, ctx, &mut cap, active_modal, setup_pending);
    if !cap.captured.is_empty() {
        if let Err(error) = atomcode_daemon::native_live::publish_command_output(format!(
            "/{}\n{}",
            cmd.trim_start_matches('/'),
            cap.captured
        )) {
            cap.inner
                .render(UiLine::Error(format!("斜杠命令输出同步失败：{error:?}")));
            cap.inner.flush();
        }
    }
    result
}

/// 中继客户端 oss 下载地址。
/// 对应 gitcode.com/atomgit_atomcode/atomcode-relay-release 仓库的 Release。
const RELAY_CLIENT_DOWNLOAD_BASE: &str =
    "https://gitcode.com/atomgit_atomcode/atomcode-relay-release/releases/download";

/// relay-client 版本清单地址。
const RELAY_MANIFEST_URL: &str =
    "https://raw.gitcode.com/atomgit_atomcode/atomcode-relay-release/raw/main/relay-latest.json";

/// 兜底版本号（远端清单获取失败时使用，与 release 版本保持一致）。
const FALLBACK_RELAY_VERSION: &str = "v0.1.0";

/// 兜底版本的 sha256 和 size（远端清单获取失败时使用）。
/// 各平台值从 relay-latest.json 同步。
const FALLBACK_BINARIES: &[(&str, &str, u64)] = &[
    (
        "aarch64-macos",
        "a3eb823821cc29526371aa11f0f03f08e0fe9089300d3d7e81b19d0d848ca78a",
        4577584,
    ),
    (
        "x86_64-macos",
        "eb77bd0e6f46ec6dbe8f7dcbafe814d3d0992ca26e5c6b05182349aa6f59ad03",
        4916448,
    ),
    (
        "x86_64-linux",
        "37725dfd94ab58efe619b6f8e087db40c9a456b6d87c075c409c9a2ce83e0e94",
        5263216,
    ),
    (
        "aarch64-linux",
        "e63d374daf27f7743fc28624bdd4fcfae04d011566bd42175291df5f4abcbd7d",
        4661464,
    ),
    (
        "ohos-arm64",
        "a5082c219aaea7114758774b9c9e4924c84c9fb16b39fe9f92e6c7ab083d0744",
        4646656,
    ),
    (
        "x86_64-win",
        "9819fad219bb743af036a134ff903de8c2469bcffe7a655548c2229edb5f398e",
        5683344,
    ),
];

/// relay-client 版本清单结构。
#[derive(serde::Deserialize)]
struct RelayManifest {
    version: String,
    binaries: std::collections::BTreeMap<String, RelayBinaryEntry>,
}

#[derive(serde::Deserialize)]
struct RelayBinaryEntry {
    sha256: String,
    size: u64,
}

/// 获取 relay-client 远端版本清单。
async fn fetch_relay_manifest() -> Result<RelayManifest, String> {
    let token = atomcode_auth::oauth::get_valid_token()
        .map_err(|_| "未登录 GitCode。请先在 atomcode 中执行 /login 登录账号".to_string())?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("atomcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;

    let resp = client
        .get(RELAY_MANIFEST_URL)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .map_err(|e| format!("获取版本清单失败：{e}"))?;

    if !resp.status().is_success() {
        return Err(format!("获取版本清单返回 HTTP {}", resp.status().as_u16()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("读取版本清单失败：{e}"))?;
    let manifest: RelayManifest =
        serde_json::from_str(&body).map_err(|e| format!("解析版本清单失败：{e}"))?;

    Ok(manifest)
}

/// 检测当前平台对应的目标标识，用于构建下载文件名。
/// 格式：{arch}-{os}，与 Release 实际文件名一致。
fn relay_client_target() -> &'static str {
    // HarmonyOS / OpenHarmony 在运行时 OS 显示为 "linux"，
    // 用编译时 cfg 区分
    #[cfg(target_env = "ohos")]
    {
        return match std::env::consts::ARCH {
            "aarch64" | "arm64" => "ohos-arm64",
            _ => "unknown",
        };
    }
    #[cfg(not(target_env = "ohos"))]
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-macos",
        ("macos", "x86_64") => "x86_64-macos",
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        ("windows", "x86_64") => "x86_64-win",
        _ => "unknown",
    }
}

/// 根据平台名构建下载文件名（含版本号，Windows 加 .exe 后缀）。
fn relay_client_filename(target: &str, version: &str) -> String {
    if target.starts_with("x86_64-win") {
        format!("atomcode-relay-client-{}-{}.exe", version, target)
    } else {
        format!("atomcode-relay-client-{}-{}", version, target)
    }
}

/// 字节数组转小写 hex 字符串。
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

/// 解析 semver 版本号 `vMAJOR.MINOR.PATCH`，返回 (major, minor, patch)。
/// 无法解析时返回 None。
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().strip_prefix('v')?.split('-').next()?;
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    Some((
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ))
}

/// 判断 latest 是否比 current 新（semver 比较）。
fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => latest.trim() != current.trim(),
    }
}

/// 解析 relay-client 二进制路径。优先级：
/// 1. `ATOMCODE_RELAY_CLIENT_BIN` 环境变量 —— 开发者/特殊部署覆盖。
/// 2. 与 atomcode 自身可执行文件同目录 —— 安装包捆绑分发。
fn resolve_relay_client_bin() -> Option<String> {
    // 1) 显式环境变量覆盖（非空才采纳）。
    if let Ok(p) = std::env::var("ATOMCODE_RELAY_CLIENT_BIN") {
        if !p.is_empty() && std::path::Path::new(&p).is_file() {
            return Some(p);
        }
    }

    // 2) 与自身同目录。Windows 带 .exe 后缀；命中文件才返回绝对路径。
    let exe_name = if cfg!(windows) {
        "atomcode-relay-client.exe"
    } else {
        "atomcode-relay-client"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|dir| dir.join(exe_name)) {
            if sibling.is_file() {
                return Some(sibling.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// relay-client 的缓存目录：`$ATOMCODE_HOME/bin`。
///
/// 走 `Config::config_dir()` 而不是硬拼 `~/.atomcode`：设了 `$ATOMCODE_HOME`
/// 时,下载的二进制本该和其它数据落在同一棵树里 —— 否则 `uninstall` 扫不到它,
/// 而且提示语指的目录和实际写入的目录会对不上。
fn relay_client_cache_dir() -> PathBuf {
    atomcode_config::config::Config::config_dir().join("bin")
}

/// 确保 relay-client 二进制可用。
/// 先尝试本地查找（环境变量 → 同目录 → 缓存），都不存在则自动下载到缓存目录。
fn ensure_relay_client_bin() -> Result<String, String> {
    // 先尝试环境变量和同目录
    if let Some(bin) = resolve_relay_client_bin() {
        return Ok(bin);
    }

    let bare_name = if cfg!(windows) {
        "atomcode-relay-client.exe"
    } else {
        "atomcode-relay-client"
    };

    let cache_dir = relay_client_cache_dir();
    let cache_path = cache_dir.join(bare_name);
    let version_path = cache_dir.join(".version");

    // 缓存已存在 → 直接使用
    if cache_path.is_file() {
        return Ok(cache_path.to_string_lossy().into_owned());
    }

    // 跳过下载标志
    if std::env::var("ATOMCODE_RELAY_CLIENT_SKIP_DOWNLOAD").is_ok_and(|v| v == "1") {
        return Err(format!(
            "自动下载已禁用（ATOMCODE_RELAY_CLIENT_SKIP_DOWNLOAD=1），\
             请手动将 relay-client 放入 {} 目录",
            cache_dir.display()
        ));
    }

    // 检测平台
    let target = relay_client_target();
    if target == "unknown" {
        return Err(format!(
            "不支持的平台：{}/{}。请手动编译 relay-client 并放到 {} 中",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cache_dir.display()
        ));
    }

    // 6) 获取远端版本清单（含最新版本号 + sha256）
    let manifest = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(fetch_relay_manifest())
    });
    let manifest = match manifest {
        Ok(m) => m,
        Err(_) => {
            // 清单获取失败 → 使用兜底版本
            // 有缓存且版本不低于兜底版本 → 直接用缓存
            if cache_path.is_file() {
                if let Ok(ref ver) = std::fs::read_to_string(&version_path) {
                    let ver = ver.trim();
                    // 兜底版本作为最低要求，用 semver 比对
                    if !is_newer_version(FALLBACK_RELAY_VERSION, ver) {
                        return Ok(cache_path.to_string_lossy().into_owned());
                    }
                }
                // 缓存版本低于兜底版本 → 继续走兜底下载
            }
            // 构造兜底 manifest
            let mut fallback_binaries = std::collections::BTreeMap::new();
            for (platform, sha256, size) in FALLBACK_BINARIES {
                fallback_binaries.insert(
                    platform.to_string(),
                    RelayBinaryEntry {
                        sha256: sha256.to_string(),
                        size: *size,
                    },
                );
            }
            RelayManifest {
                version: FALLBACK_RELAY_VERSION.to_string(),
                binaries: fallback_binaries,
            }
        }
    };

    // 7) 检查缓存版本是否最新
    let cached_version = std::fs::read_to_string(&version_path).ok();
    if let Some(ref ver) = cached_version {
        let ver = ver.trim();
        if !is_newer_version(&manifest.version, ver) && cache_path.is_file() {
            return Ok(cache_path.to_string_lossy().into_owned());
        }
    }

    // 8) 获取当前平台的 binary entry
    let entry = match manifest.binaries.get(target) {
        Some(e) => e,
        None => {
            return Err(format!(
                "版本 {} 不支持当前平台 {}",
                manifest.version, target
            ));
        }
    };

    // 9) 自动下载 + SHA256 校验
    let filename = relay_client_filename(target, &manifest.version);
    let url = format!(
        "{}/{}/{}",
        RELAY_CLIENT_DOWNLOAD_BASE, manifest.version, filename
    );

    // 使用 block_in_place 执行异步下载（当前在同步上下文中）
    let download_result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(download_relay_client(
            &url,
            &cache_path,
            &entry.sha256,
            entry.size,
        ))
    });

    match download_result {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&cache_path, std::fs::Permissions::from_mode(0o755));
            }
            // 写入缓存版本号
            let _ = std::fs::write(&version_path, manifest.version.as_bytes());
            Ok(cache_path.to_string_lossy().into_owned())
        }
        Err(e) => {
            let msg = format!(
                "自动下载 relay-client 失败：{}\n\
                 \n\
                 安全下载：\n\
                 1. 打开浏览器访问\n\
                    https://gitcode.com/atomgit_atomcode/atomcode-relay-release/releases\n\
                 2. 下载对应平台的 binary\n\
                 3. 保存到 {cache}/atomcode-relay-client\n\
                 4. chmod +x {cache}/atomcode-relay-client\n\
                 5. /app 重试\n\
                 \n\
                 快速安装：\n\
                 curl -fsSL https://raw.gitcode.com/atomgit_atomcode/atomcode-relay-release/raw/main/scripts/install.sh | sh\n\
                 && /app 重试",
                e,
                cache = cache_dir.display()
            );
            Err(msg)
        }
    }
}

/// 从指定 URL 下载 relay-client 二进制到缓存路径。
/// 使用 GitCode OAuth token 进行鉴权，下载完成后校验 SHA256 和文件大小。
async fn download_relay_client(
    url: &str,
    dest: &std::path::Path,
    expected_sha256: &str,
    expected_size: u64,
) -> Result<(), String> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    // SHA256 计算
    use sha2::{Digest, Sha256};

    // 确保缓存目录存在
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建缓存目录失败：{e}"))?;
    }

    // 获取 GitCode OAuth token（用户需先 /login）
    let token = atomcode_auth::oauth::get_valid_token()
        .map_err(|_| "未登录 GitCode。请先在 atomcode 中执行 /login 登录账号".to_string())?;

    // 构建 HTTP 客户端 + 添加鉴权头
    let client = reqwest::Client::builder()
        .user_agent(concat!("atomcode/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("创建 HTTP 客户端失败：{e}"))?;

    let resp = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {}", token))
        .send()
        .await
        .map_err(|e| format!("下载请求失败：{e}（请检查网络连接）"))?;

    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err("GitCode 鉴权失败，token 可能已过期。请重新执行 /login 登录".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!(
            "下载返回 HTTP {}（Release 可能不存在或无权访问）",
            resp.status().as_u16()
        ));
    }

    // 流式下载 + SHA256 累积
    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("创建文件失败：{e}"))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("读取下载流失败：{e}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写入文件失败：{e}"))?;
        written += chunk.len() as u64;
    }
    file.flush().await.map_err(|e| format!("刷盘失败：{e}"))?;
    drop(file);

    // 校验文件大小
    if expected_size > 0 && written != expected_size {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "文件大小不匹配：预期 {} 字节，实际下载 {} 字节",
            expected_size, written
        ));
    }

    // 校验 SHA256
    let got = hex_encode(&hasher.finalize());
    if !got.eq_ignore_ascii_case(expected_sha256) {
        let _ = std::fs::remove_file(dest);
        return Err(format!(
            "SHA256 校验失败：\n  预期: {}\n  实际: {}",
            expected_sha256, got
        ));
    }

    Ok(())
}

fn execute_slash_command_impl(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
    setup_pending: &mut bool,
) -> Result<()> {
    // Built-in commands are all lowercase ASCII; normalise the user's
    // input so `/SESSION`, `/Session`, `/sEssIon` all hit the same arm
    // as `/session`. `arg` is left untouched — paths / URLs are
    // case-sensitive in general. Aliases (e.g. `/new`) then resolve to their
    // canonical command name here, so `/new` hits the `session` arm without a
    // dedicated match arm — add the alias to COMMAND_ALIASES only.
    let cmd_lower = cmd.to_ascii_lowercase();
    let cmd = crate::commands::canonical_command_name(&cmd_lower);

    // Emit use_command telemetry before dispatch so the event fires
    // regardless of whether the command succeeds or errors out.
    {
        use atomcode_telemetry::Event;
        let cmd_name = cmd.trim_start_matches('/').to_string();
        ctx.telemetry.track(Event::UseCommand {
            type_: cmd_name,
            success: Some(true),
            error_kind: None,
            error_data: None,
        });
    }

    match cmd {
        "quit" | "exit" => {
            super::arm_shutdown_watchdog(ctx);
        }
        "copy" => {
            // Copy a fenced code block from the most recent assistant reply to
            // the system clipboard, VERBATIM — terminal-native selection copies
            // the hard-wrapped + PAD-indented body cells, which breaks long
            // commands; this reads the original markdown instead.
            //   /copy        → the last code block (the command just shown)
            //   /copy N      → the Nth code block (1-based)
            //   /copy all    → every code block, blank-line separated
            match resolve_copy(&state.last_assistant_response, arg) {
                CopyResolve::NoBlocks => {
                    renderer.render(UiLine::Warning(t(Msg::CopyNoCodeBlock).into_owned()));
                }
                CopyResolve::EmptyMsg => {
                    renderer.render(UiLine::Warning(t(Msg::CopyMsgEmpty).into_owned()));
                }
                CopyResolve::BadIndex(count) => {
                    renderer.render(UiLine::Warning(t(Msg::CopyBadIndex { count }).into_owned()));
                }
                CopyResolve::Text(payload, is_msg) => {
                    let lines = payload.lines().count().max(1);
                    let chars = payload.chars().count();
                    if copy_text_to_clipboard_osc52(&payload) {
                        let msg = if is_msg {
                            t(Msg::CopyOkMsg { lines, chars })
                        } else {
                            t(Msg::CopyOk { lines, chars })
                        };
                        renderer.render(UiLine::CommandOutput(msg.into_owned()));
                    } else {
                        renderer.render(UiLine::Error(t(Msg::CopyFailed).into_owned()));
                    }
                }
            }
            renderer.flush();
        }
        "save" => {
            // Export the full current conversation (every real user prompt +
            // assistant reply, in order) to a local markdown file.
            //   /save            → <working-dir>/atomcode-session-YYYYMMDD-HHMMSS.md
            //   /save report.md  → <working-dir>/report.md
            //   /save /abs/x.md  → absolute path
            // Existing files are overwritten; missing parent dirs are an error.
            match resolve_save_in(&ctx.current_session.messages, arg, &ctx.working_dir) {
                SaveOutcome::Ok(path) => {
                    let path_str = path.to_string_lossy();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::SaveOk { path: &path_str }).into_owned(),
                    ));
                }
                SaveOutcome::EmptyHistory => {
                    renderer.render(UiLine::Warning(t(Msg::SaveEmpty).into_owned()));
                }
                SaveOutcome::IoError(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::SaveIoError { error: &e }).into_owned(),
                    ));
                }
                SaveOutcome::InvalidPath(p) => {
                    renderer.render(UiLine::Error(
                        t(Msg::SaveInvalidPath { path: &p }).into_owned(),
                    ));
                }
                SaveOutcome::RefuseOverwrite(p) => {
                    renderer.render(UiLine::Error(
                        t(Msg::SaveRefuseOverwrite { path: &p }).into_owned(),
                    ));
                }
            }
            renderer.flush();
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
                    let args_tag = match cmd.args_requirement {
                        ArgsRequirement::Required => " <args>",
                        ArgsRequirement::Optional => " [args]",
                        ArgsRequirement::None => "",
                    };
                    out.push_str(&format!(
                        "    /{}{}  — {} ({})\n",
                        cmd.name, args_tag, cmd.description, source_label
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
                    submit_agent_turn(ctx, state, rendered);
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
                        let ev = match atomcode_capabilities::plugin::installer::ensure_plugin_installed(
                            "atomcode",
                            "atomcode-skills",
                            "https://atomgit.com/atomgit_atomcode/atomcode-skills.git",
                        ) {
                            Ok(info) => {
                                atomcode_capabilities::plugin::PluginJobEvent::PluginInstalled(info)
                            }
                            Err(e) => {
                                if let Some(_aie) = e.downcast_ref::<
                                    atomcode_capabilities::plugin::installer::AlreadyInstalledError,
                                >() {
                                    atomcode_capabilities::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                        id: _aie.id.clone(),
                                    }
                                } else {
                                    atomcode_capabilities::plugin::PluginJobEvent::Failed {
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
            renderer.render(UiLine::CommandOutput(t(Msg::KeybindingsHelp).into_owned()));
            renderer.flush();
        }
        "view" => {
            let trimmed = arg.trim();
            if trimmed.is_empty() {
                // No path → open the files-only fuzzy picker.
                *active_modal = Some(Box::new(FileViewer::open_picker(ctx.working_dir.clone())));
            } else {
                // Resolve `~`, absolute, and project-relative paths so files
                // OUTSIDE the project open too.
                let path = resolve_view_path(trimmed, &ctx.working_dir);
                match FileViewer::open(&path) {
                    Ok(viewer) => {
                        *active_modal = Some(Box::new(viewer));
                    }
                    Err(e) => {
                        renderer.render(UiLine::Error(format!("{}", e)));
                        renderer.flush();
                    }
                }
            }
        }
        "plan" => {
            state.agent_mode = AgentMode::Plan;
            ctx.runtime
                .dispatch(atomcode_coding::DriverCommand::SetMode(
                    atomcode_coding::RuntimeMode::Plan,
                ))
                .ok();
            atomcode_daemon::live_set_mode(AgentMode::Plan);
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSwitchedPlanMode).into_owned(),
            ));
            renderer.flush();
        }
        "build" => {
            state.agent_mode = AgentMode::Build;
            state.build_badge_visible = true;
            ctx.runtime
                .dispatch(atomcode_coding::DriverCommand::SetMode(
                    atomcode_coding::RuntimeMode::Build,
                ))
                .ok();
            atomcode_daemon::live_set_mode(AgentMode::Build);
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSwitchedBuildMode).into_owned(),
            ));
            renderer.flush();
        }
        "auto" => {
            state.agent_mode = AgentMode::Auto;
            ctx.runtime
                .dispatch(atomcode_coding::DriverCommand::SetMode(
                    atomcode_coding::RuntimeMode::Auto,
                ))
                .ok();
            atomcode_daemon::live_set_mode(AgentMode::Auto);
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSwitchedAutoMode).into_owned(),
            ));
            renderer.flush();
        }
        "review" => {
            // Trigger the coding agent's `code_review` sub-agent tool. Map the optional
            // arg to the tool's scope (default = working-tree changes; `staged`; or a base
            // ref), then the model calls the tool and summarizes its findings. If the
            // configured runtime lacks the tool, the model simply says so.
            submit_agent_turn(ctx, state, review_prompt(arg));
        }
        "config" => {
            *active_modal = Some(Box::new(ConfigPanel::open()));
            return Ok(());
        }
        "reload" => {
            match reload_persisted_config(ctx) {
                Ok(PersistedConfigReload::Applied { provider, model }) => {
                    state.on_model_window_changed(ctx.config.default_context_window());
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CmdReloadDone {
                            provider: &provider,
                            model: &model,
                        })
                        .into_owned(),
                    ));
                }
                Ok(PersistedConfigReload::Queued | PersistedConfigReload::Joined) => {}
                Err(e) => {
                    let msg = e.to_string();
                    renderer.render(UiLine::Error(
                        t(Msg::CmdReloadFailed { error: &msg }).into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        "clear" => {
            // `/clear` starts a fresh conversation (matches Claude Code and the
            // common expectation): it was previously a SCREEN-ONLY wipe, so the
            // engine kept the full history and the model still "remembered"
            // everything after a clear. Delegate to the same reset `/session`
            // uses — it sends ClearConversation to the engine AND wipes the
            // screen + re-renders the welcome banner.
            reset_to_new_session(ctx, state, renderer);
        }
        "session" => {
            // Start fresh in the current directory. Ports `/session` from the
            // legacy TUI. Shared with the webui-driven project switch via
            // `reset_to_new_session`.
            reset_to_new_session(ctx, state, renderer);
        }
        "model" => {
            if !crate::modals::model_picker::has_selectable_models(&ctx.config) {
                renderer.render(UiLine::CommandOutput(t(Msg::CmdNoProviders).into_owned()));
                renderer.flush();
            } else {
                *active_modal = Some(Box::new(ModelPicker::open(&ctx.config)));
            }
        }
        "language" => {
            if arg.is_empty() {
                *active_modal = Some(Box::new(LanguagePicker::open()));
            } else {
                match arg.parse::<atomcode_config::locale::Locale>() {
                    Ok(locale) => {
                        save_language_and_reload(ctx, locale, renderer);
                    }
                    Err(_) => {
                        let msg = t(Msg::ErrUnsupportedLocale { input: arg });
                        renderer.render(UiLine::CommandOutput(format!("  {msg}\n")));
                        renderer.flush();
                    }
                }
            }
        }
        "resume" => {
            // The catalog scan reads/parses every session file, which froze the UI
            // when done inline (thousands of files across projects). Offload it to a
            // blocking thread and install the picker via an event when it lands —
            // mirroring the async session-resume path. `install_pending_session_picker`
            // in the main loop consumes the result.
            renderer.render(UiLine::CommandOutput(
                t(Msg::CmdSessionListLoading).into_owned(),
            ));
            renderer.flush();
            let working_dir = ctx.working_dir.clone();
            let event_working_dir = working_dir.clone();
            let event_tx = ctx.runtime_event_tx.clone();
            let runtime_id = ctx.foreground_runtime_id;
            tokio::spawn(async move {
                let scanned = tokio::task::spawn_blocking(move || {
                    atomcode_daemon::legacy_convert::catalog_for_project(&working_dir)
                        .map(|all| {
                            all.into_iter()
                                .filter(|entry| entry.message_count > 0)
                                .map(crate::session::SessionMeta::from)
                                .collect::<Vec<_>>()
                        })
                        .map_err(|e| e.to_string())
                })
                .await;
                let result = match scanned {
                    Ok(inner) => inner,
                    Err(join) => Err(join.to_string()),
                };
                let _ = event_tx.send(crate::event_loop::bg_runtime::RuntimeEvent {
                    runtime_id,
                    event: crate::event_loop::bg_runtime::RuntimeEventPayload::Driver(
                        crate::event_loop::bg_runtime::DriverEvent::SessionCatalogLoaded {
                            working_dir: event_working_dir,
                            result,
                        },
                    ),
                });
            });
        }
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
                let new_name = arg.trim().to_string();
                let project_bucket = active_session_project_bucket(&ctx.working_dir);
                match perform_session_rename(&project_bucket, &ctx.current_session.id, &new_name) {
                    Ok((old_name, _)) => {
                        ctx.current_session.rename(new_name.clone());
                        ctx.bg_manager.set_foreground_session(
                            ctx.current_session.clone(),
                            ctx.working_dir.clone(),
                        );
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::SessionRenamed {
                                old: &old_name,
                                new: &new_name,
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    }
                    Err(e) => {
                        renderer.render(UiLine::Error(
                            t(Msg::SessionSaveFailed {
                                error: &e.to_string(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    }
                }
            }
        }
        "provider" => {
            *active_modal = Some(Box::new(crate::modals::ProviderPanel::open()));
        }
        "proxy" => {
            *active_modal = Some(Box::new(ProxyPicker::open(&ctx.config)));
        }
        "status" => {
            // Interactive `/status` shows the Proxy line (after CodingPlan); the
            // remote/phone view omits it. Order is owned by `assemble_status`.
            let proxy = format!("  Proxy:  {}\n", ctx.config.network.proxy.summary());
            let txt = build_status_text(ctx, Some(&proxy));
            if matches!(state.phase, crate::state::UiPhase::Streaming) && !ctx.is_plain_renderer {
                // Mid-turn: keep the report in the footer snapshot below the input
                // box (like `/usage` and `/cost`) instead of injecting it into
                // conversation scrollback, where live tool output would interleave
                // with it. Drop any live `/usage` panel so its tab keys don't steer
                // a report that's no longer on screen.
                state.footer_usage = None;
                state.footer_command_output = Some(txt);
            } else {
                renderer.render(UiLine::CommandOutput(txt));
                renderer.flush();
            }
        }
        "diff" => {
            if matches!(state.phase, crate::state::UiPhase::Streaming) && !ctx.is_plain_renderer {
                // Mid-turn: footer snapshot, not scrollback (see `/status`). The
                // error text folds into the same snapshot so a failed diff still
                // reports below the input box.
                state.footer_usage = None;
                state.footer_command_output = Some(build_diff_stat_text(ctx).unwrap_or_else(|e| e));
            } else if ctx.is_plain_renderer || !matches!(state.phase, crate::state::UiPhase::Idle) {
                match build_diff_stat_text(ctx) {
                    Ok(text) => renderer.render(UiLine::CommandOutput(text)),
                    Err(error) => renderer.render(UiLine::Error(error)),
                }
                renderer.flush();
            } else {
                *active_modal = Some(Box::new(DiffViewer::open(
                    ctx.working_dir.clone(),
                    ctx.wake_tx.clone(),
                )));
            }
        }
        "undo" => {
            dispatch_undo(arg, state, ctx, renderer);
        }
        "rewind" => {
            dispatch_rewind(state, ctx, renderer);
        }
        "usage" => {
            // Mid-turn (Streaming): keep a text snapshot in the footer directly
            // below the input box. It must not enter conversation scrollback,
            // and Esc dismisses it without cancelling the running turn.
            // Idle: open the interactive modal.
            if matches!(state.phase, crate::state::UiPhase::Streaming) {
                // Fetch the FULL dataset (overview + models) so the footer report
                // is tab-switchable mid-stream — the interactive modal can't
                // install here (live token redraws own the footer), so we stash
                // the panel and re-render its active tab in place on each tab
                // key. Two gateway calls, once per `/usage`; switching tabs is
                // then purely local.
                match fetch_usage_data() {
                    Some(data) => {
                        let panel = UsageModal::new(data);
                        state.footer_command_output = Some(
                            panel.active_snapshot_text(ctx.caps.colors, ctx.caps.unicode_symbols),
                        );
                        state.footer_usage = Some(panel);
                    }
                    None => {
                        state.footer_command_output =
                            Some(t(Msg::UsageCodingPlanOnly).into_owned());
                        state.footer_usage = None;
                    }
                }
            } else {
                open_usage(renderer, active_modal);
            }
        }
        "cost" => {
            let text = build_session_cost_text(ctx, state);
            if matches!(state.phase, crate::state::UiPhase::Streaming) {
                // `/cost` is a static report — drop any live `/usage` panel so
                // tab keys don't steer a report that's no longer on screen.
                state.footer_usage = None;
                state.footer_command_output = Some(text);
            } else {
                renderer.render(UiLine::CommandOutput(text));
                renderer.flush();
            }
        }
        "schedule" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let tasks = atomcode_config::schedule::list();
            let text = build_schedule_list_text(&tasks, now);
            if matches!(state.phase, crate::state::UiPhase::Streaming) {
                state.footer_command_output = Some(text);
            } else {
                renderer.render(UiLine::CommandOutput(text));
                renderer.flush();
            }
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
            if let Err(error) = request_context_stats_render(
                &ctx.runtime,
                ctx.foreground_runtime_id,
                ctx.runtime_event_tx.clone(),
                &mut state.pending_context_render,
                show_prompt,
            ) {
                renderer.render(UiLine::Error(format!(
                    "refresh context stats could not be started: {error}"
                )));
                renderer.flush();
            }
        }
        "compact" => {
            let focus = (!arg.trim().is_empty()).then(|| arg.trim().to_string());
            if let Err(error) = ctx.runtime.compact(focus) {
                renderer.render(UiLine::Error(error.to_string()));
                renderer.flush();
            }
        }
        "remember" => {
            let text = arg.trim();
            if text.is_empty() {
                renderer.render(UiLine::Error(t(Msg::RememberUsage).into_owned()));
                renderer.flush();
            } else {
                let (content, global) = if let Some(rest) = text.strip_prefix("--global ") {
                    (rest.trim().to_string(), true)
                } else {
                    (text.to_string(), false)
                };
                if content.is_empty() {
                    renderer.render(UiLine::Error(t(Msg::RememberUsage).into_owned()));
                } else {
                    let store = if global {
                        MemoryStore::global()
                    } else {
                        MemoryStore::project(&ctx.working_dir)
                    };
                    let scope = if global { "global" } else { "project" };
                    // Dedup on write (parity with the model-facing `memory` tool) so a
                    // repeated /remember of the same line doesn't double-write.
                    match store.append_deduped(&content) {
                        Ok(true) => renderer.render(UiLine::CommandOutput(format!(
                            "Remembered ({scope}): {content}"
                        ))),
                        Ok(false) => renderer.render(UiLine::CommandOutput(format!(
                            "Already remembered ({scope}): {content}"
                        ))),
                        Err(e) => {
                            renderer.render(UiLine::Error(format!("Failed to remember: {e}")))
                        }
                    }
                }
                renderer.flush();
            }
        }
        "forget" => {
            let keyword = arg.trim();
            if keyword.is_empty() {
                renderer.render(UiLine::Error(t(Msg::ForgetUsage).into_owned()));
            } else {
                let mut removed = MemoryStore::project(&ctx.working_dir)
                    .remove_matching(keyword)
                    .unwrap_or_default();
                removed.extend(
                    MemoryStore::global()
                        .remove_matching(keyword)
                        .unwrap_or_default(),
                );
                let msg = if removed.is_empty() {
                    format!("No memory entries matched '{keyword}'.")
                } else {
                    format!(
                        "Forgot {} entr{}.",
                        removed.len(),
                        if removed.len() == 1 { "y" } else { "ies" }
                    )
                };
                renderer.render(UiLine::CommandOutput(msg));
            }
            renderer.flush();
        }
        "memory" => {
            let global = MemoryStore::global();
            let project = MemoryStore::project(&ctx.working_dir);
            let name = ctx
                .working_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".into());
            let merged = MemoryStore::merged_for_prompt(&global, &project, &name);
            let out = if merged.trim().is_empty() {
                "(memory is empty)".to_string()
            } else {
                merged
            };
            renderer.render(UiLine::CommandOutput(out));
            renderer.flush();
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
                if let Err(error) = attach_live_runtime(ctx, state.agent_mode, renderer) {
                    renderer.render(UiLine::Error(error));
                    renderer.flush();
                    return Ok(());
                }
                let open_msg = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(
                        atomcode_daemon::ensure_server_and_open(
                            &host,
                            atomcode_daemon::WEBUI_DEFAULT_PORT,
                            true,
                        ),
                    )
                });
                open_msg
            };
            renderer.render(UiLine::CommandOutput(msg));
            renderer.flush();
        }
        "sync" => {
            if arg.trim() == "off" {
                match detach_live_runtime(ctx) {
                    Ok(true) => renderer.render(UiLine::CommandOutput("已停止共享当前会话".into())),
                    Ok(false) => {
                        renderer.render(UiLine::CommandOutput("当前未处于同步模式".to_string()))
                    }
                    Err(error) => renderer.render(UiLine::Error(error)),
                }
            } else {
                if let Err(error) = attach_live_runtime(ctx, state.agent_mode, renderer) {
                    renderer.render(UiLine::Error(error));
                }
            }
            renderer.flush();
        }
        "desktop" => {
            // Detect an installed AtomCode desktop app (new "Desktop" preferred
            // over old "Air"); launch it, or point the user at the download page.
            let home = crate::platform::home_dir().unwrap_or_default();
            let env = |k: &str| std::env::var(k).ok();
            let cands = super::desktop::candidate_apps(&home, &env);
            let line = match super::desktop::detect(&cands, |p| p.exists()) {
                Some(c) => {
                    let path = c.path.display().to_string();
                    match super::desktop::launch(c) {
                        Ok(()) => t(Msg::DesktopOpening {
                            name: c.display_name,
                            path: &path,
                        })
                        .into_owned(),
                        Err(e) => t(Msg::DesktopLaunchFailed {
                            path: &path,
                            err: &e.to_string(),
                        })
                        .into_owned(),
                    }
                }
                None => t(Msg::DesktopNotInstalled {
                    url: super::desktop::download_url(),
                })
                .into_owned(),
            };
            renderer.render(UiLine::CommandOutput(line));
            renderer.flush();
        }
        "app" => {
            // 把当前会话经【自建多租户中继】暴露给手机 App，二维码配对。
            // 与 /webui 共用当前 Coding Runtime 和 live hub，
            // 区别：① 不开浏览器，吐终端二维码；② 本机 server 走 daemon 模式
            // （无 token，仅回环绑定），鉴权边界落在中继的 route token。
            //
            // 远程访问要连中继、并从中继的发布地址下载 relay-client 二进制。
            // 没有自己中继的部署可设 ATOMCODE_ENABLE_RELAY=0 关掉整条链路。
            if !atomcode_config::endpoints::relay_enabled() {
                renderer.render(UiLine::CommandOutput(
                    "远程访问在本部署中未启用（ATOMCODE_ENABLE_RELAY=0）。".to_string(),
                ));
                renderer.flush();
                return Ok(());
            }

            // 中继地址 → (ws 拨号 URL, App 用的 https 根)。
            fn derive_relay_urls(base: &str) -> (String, String) {
                let trimmed = base.trim().trim_end_matches('/');
                // 用户可能直接给 wss://.../ws/daemon：剥掉路径还原成根。
                let https_base = if let Some(rest) = trimmed.strip_prefix("wss://") {
                    format!("https://{}", rest.trim_end_matches("/ws/daemon"))
                } else if let Some(rest) = trimmed.strip_prefix("ws://") {
                    format!("http://{}", rest.trim_end_matches("/ws/daemon"))
                } else {
                    trimmed.to_string()
                };
                let ws_url = if let Some(rest) = https_base.strip_prefix("https://") {
                    format!("wss://{rest}/ws/daemon")
                } else if let Some(rest) = https_base.strip_prefix("http://") {
                    format!("ws://{rest}/ws/daemon")
                } else {
                    // 没写 scheme：默认按 TLS 处理。
                    format!("wss://{https_base}/ws/daemon")
                };
                (ws_url, https_base)
            }
            // 最小百分号编码：query value 里除 unreserved 外全部转义，
            // App 端 Uri.queryParameters 会自动解码还原。
            fn pct(s: &str) -> String {
                let mut out = String::with_capacity(s.len() * 3);
                for b in s.bytes() {
                    match b {
                        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                            out.push(b as char)
                        }
                        _ => out.push_str(&format!("%{b:02X}")),
                    }
                }
                out
            }

            let a = arg.trim();
            let msg = if a == "stop" {
                // Remote access must stop immediately even when the live session
                // is busy. A failed handoff keeps only the TUI attached so it can
                // continue receiving the in-flight turn safely.
                let detach_error = detach_live_runtime(ctx).err();
                let killed = ctx
                    .app_relay_child
                    .take()
                    .map(|mut c| {
                        let _ = c.start_kill();
                    })
                    .is_some();
                let server_stopped = atomcode_daemon::stop_app_server();
                let mut output = if killed || server_stopped {
                    "已停止 App 远程访问".to_string()
                } else {
                    "App 远程访问未在运行".to_string()
                };
                if let Some(error) = detach_error {
                    output.push_str(&format!("\n{error}；TUI 暂时保持同步"));
                }
                output
            } else {
                // 部署默认中继。用户直接敲 `/app` 即可，无需选择/配置中继地址。
                // 中继地址：命令参数 > endpoints（含 ATOMCODE_APP_RELAY 覆盖）。
                let relay_base = if a.is_empty() {
                    Some(atomcode_config::endpoints::relay_url().to_string())
                } else {
                    Some(a.trim_start_matches("--relay").trim().to_string())
                };
                match relay_base.filter(|s| !s.is_empty()) {
                    // 仅在显式给了空参数（如 `/app --relay`）时可达。
                    None => "用法：/app（默认连官方中继），或 /app <中继地址> 覆盖".to_string(),
                    Some(relay) => {
                        // 1) 检查登录态：未登录不允许开启远程访问。
                        if atomcode_auth::oauth::get_stored_auth().is_none() {
                            renderer.render(UiLine::CommandOutput(
                                "远程访问需要先登录。输入 /login 完成登录后，再执行 /app。"
                                    .to_string(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        // 2) 起本机 App server（daemon 模式、不开浏览器、回环绑定）。
                        //    每次 /app 都重建 server，确保 app_user_id 始终是当前登录用户。
                        //    Keep any current sync attachment until startup succeeds;
                        //    attach_live_session replaces it atomically on the success path.
                        atomcode_daemon::stop_app_server();
                        //    传入当前登录 user_id 启用双向校验。
                        let app_user_id =
                            atomcode_auth::oauth::get_stored_auth().map(|a| a.user.id);
                        let started = tokio::task::block_in_place(|| {
                            tokio::runtime::Handle::current().block_on(
                                atomcode_daemon::ensure_app_server(
                                    "127.0.0.1",
                                    atomcode_daemon::APP_DEFAULT_PORT,
                                    app_user_id,
                                ),
                            )
                        });
                        match started {
                            Err(e) => format!("App server 启动失败：{e}"),
                            Ok((_h, port)) => {
                                // 3) route token（中继路由 key + 凭证）+ 中继 URL。
                                // token = user_id.随机hex，App 端扫码后校验 user_id 是否一致。
                                let token = match atomcode_auth::oauth::get_stored_auth() {
                                    Some(auth) => format!(
                                        "{}.{}",
                                        auth.user.id,
                                        uuid::Uuid::new_v4().simple()
                                    ),
                                    None => format!(
                                        "{}{}",
                                        uuid::Uuid::new_v4().simple(),
                                        uuid::Uuid::new_v4().simple()
                                    ),
                                };
                                let (ws_url, https_base) = derive_relay_urls(&relay);
                                let machine = std::env::var("HOSTNAME")
                                    .ok()
                                    .or_else(|| std::env::var("COMPUTERNAME").ok());
                                // 4) 确保 relay-client 二进制可用（查找本地或自动下载），
                                //    然后拉起子进程。自身即 daemon，故
                                //    --no-supervise-daemon。kill_on_drop：TUI 退出随之清理。
                                let daemon_url = format!("http://127.0.0.1:{port}");
                                let spawn_result = match ensure_relay_client_bin() {
                                    Err(e) => format!("启动 relay-client 失败：{e}"),
                                    Ok(bin) => {
                                        let mut cmd = tokio::process::Command::new(&bin);
                                        cmd.arg("run")
                                            .arg("--relay")
                                            .arg(&ws_url)
                                            .arg("--token")
                                            .arg(&token)
                                            .arg("--daemon")
                                            .arg(&daemon_url)
                                            .arg("--supervise-daemon")
                                            .arg("false")
                                            .kill_on_drop(true)
                                            .stdout(std::process::Stdio::null())
                                            .stderr(std::process::Stdio::null());
                                        if let Some(m) = &machine {
                                            cmd.arg("--machine-name").arg(m);
                                        }
                                        if let Some(secret) =
                                            std::env::var("ATOMCODE_APP_RELAY_SECRET")
                                                .ok()
                                                .or_else(|| {
                                                    std::env::var("ATOM_RELAY_REGISTER_SECRET").ok()
                                                })
                                                .filter(|s| !s.is_empty())
                                        {
                                            cmd.arg("--register-secret").arg(secret);
                                        }
                                        match cmd.spawn() {
                                            Err(e) => format!(
                                                "启动 relay-client 失败（{e}）。已尝试路径 `{bin}`。\
                                                 请确认 relay-client 在 {cache} 目录下，\
                                                 或删除该目录后重试 /app 自动下载。",
                                                cache = relay_client_cache_dir().display()
                                            ),
                                            Ok(child) => {
                                                if let Some(mut old) = ctx.app_relay_child.take() {
                                                    let _ = old.start_kill();
                                                }
                                                ctx.app_relay_child = Some(child);
                                                // 5) 配对 URI（App 扫码解析 r= / t= / m=）。
                                                let m_param = machine
                                                    .as_deref()
                                                    .map(|m| format!("&m={}", pct(m)))
                                                    .unwrap_or_default();
                                                let pair_uri = format!(
                                                    "atomcode-link://pair?r={}&t={}{}",
                                                    pct(&https_base),
                                                    token,
                                                    m_param
                                                );
                                                // 6) 手机视图复用 TUI 当前 CodingRuntime。
                                                if let Err(error) = attach_live_runtime(ctx, state.agent_mode, renderer) {
                                                    if let Some(mut child) = ctx.app_relay_child.take() {
                                                        let _ = child.start_kill();
                                                    }
                                                    return Err(anyhow::anyhow!(error));
                                                }
                                                use base64::Engine;
                                                let encoded = base64::engine::general_purpose::STANDARD
                                                    .encode(pair_uri.as_bytes());
                                                match crate::render::qr::render_login_qr(
                                                    &pair_uri,
                                                    crate::render::qr::QrStyle::Dense1x2,
                                                ) {
                                                    Some(q) => format!(
                                                        "📱 使用 GitCode App 连接\n\
                                                        \n\
                                                        1. 在手机应用商店搜索「GitCode」下载最新版 App\n\
                                                        2. 打开 App → 首页 → AtomCode 模块 → 扫一扫\n\
                                                        3. 对准下方二维码即可配对连接\n\
                                                        \n\
                                                        {q}\n\
                                                        \n\
                                                        也可复制以下口令在App中连接：\n\
                                                        {encoded}\n\
                                                        \n\
                                                        （/app stop 断开连接）"
                                                    ),
                                                    None => format!(
                                                        "配对链接（二维码生成失败，手动填）：{pair_uri}"
                                                    ),
                                                }
                                            }
                                        }
                                    }
                                };
                                spawn_result
                            }
                        }
                    }
                }
            };
            renderer.render(UiLine::CommandOutput(msg));
            renderer.flush();
        }
        "login" => {
            run_login_flow(renderer, ctx)?;
        }
        "logout" => {
            // Provider config is a user asset and stays in config.toml. Logout removes
            // credentials first, then asks the runtime owner to destroy the live provider;
            // a later /login can reassemble it without losing the user's provider choice.
            //
            // 安全：登出时自动关闭 App 远程访问，防止隧道仍在线。
            if ctx
                .app_relay_child
                .take()
                .map_or(false, |mut c| c.start_kill().is_ok())
            {
                let _ = ctx.app_relay_child.take();
            }
            atomcode_daemon::stop_app_server();
            match atomcode_auth::logout() {
                Ok(()) => {
                    match deactivate_runtime_provider_after_logout(ctx) {
                        Ok(true) => {
                            // Runtime owner emits the completion only after the
                            // active AtomGit provider has been torn down.
                        }
                        Ok(false) => renderer
                            .render(UiLine::CommandOutput(t(Msg::CmdLogoutDone).into_owned())),
                        Err(error) => {
                            let message = format!(
                                "credentials removed, but provider deactivation failed: {error}"
                            );
                            renderer.render(UiLine::Error(
                                t(Msg::CmdLogoutFailed { error: &message }).into_owned(),
                            ));
                        }
                    }
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
            renderer.render(UiLine::CommandOutput(build_whoami_text()));
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
                match atomcode_updater::run_rollback() {
                    Ok(sum) => {
                        // Route through the event channel so rendering
                        // and "set done → exit" logic stays in one place.
                        let _ = ctx
                            .upgrade_tx
                            .send(atomcode_updater::UpgradeEvent::RolledBack {
                                exe: sum.exe,
                                backup: sum.backup,
                            });
                    }
                    Err(e) => {
                        let _ = ctx
                            .upgrade_tx
                            .send(atomcode_updater::UpgradeEvent::Failed(format!("{:#}", e)));
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
                    if let Err(e) = atomcode_updater::run_upgrade(current, force, tx.clone()).await
                    {
                        let _ = tx.send(atomcode_updater::UpgradeEvent::Failed(format!("{:#}", e)));
                    }
                });
            }
        }
        "cd" => {
            // Bare `/cd` opens a searchable project picker. The complete native
            // session catalog is the durable source; the small recent-dir ring
            // only biases its ordering. The picker's Enter-handler invokes
            // `apply_cd` itself, so there is nothing else to do here.
            if arg.is_empty() {
                *active_modal = Some(Box::new(DirPicker::open(
                    load_cd_picker_dirs(&ctx.working_dir, &ctx.recent_dirs),
                    ctx.working_dir.clone(),
                )));
                return Ok(());
            }
            let new_dir = resolve_cd(arg, &ctx.working_dir, ctx.previous_dir.as_deref());
            match new_dir {
                Ok(path) if paths_same(&path, &ctx.working_dir) => {
                    let cwd = ctx.working_dir.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::CdWorkingDir { cwd: &cwd }).into_owned(),
                    ));
                }
                Ok(path) => {
                    match apply_cd_with_effect(
                        ctx,
                        path,
                        crate::event_loop::SessionTransitionEffect::CdCommand {
                            echo: format!("/cd {arg}"),
                        },
                    ) {
                        // Success path stays silent: the transition is fast and its
                        // terminal updates the cwd; the "reconfiguring…" status is
                        // only shown by the guards when an action races a pending one.
                        Ok(_) => {}
                        Err(error) => renderer.render(UiLine::Error(error)),
                    }
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
                    if let Err(error) = ensure_bg_foreground_switch_allowed(
                        ctx.live_binding.is_some(),
                        provider_transition_pending(ctx),
                        ctx.pending_runtime_request_id.is_some(),
                    ) {
                        renderer.render(UiLine::Error(error.into()));
                        renderer.flush();
                        return Ok(());
                    }
                    sync_bg_foreground(ctx);
                    if !ctx.bg_manager.has_capacity() {
                        renderer.render(UiLine::Error(
                            t(Msg::BgSlotLimitReached {
                                max: bg_runtime::MAX_BACKGROUND_SLOTS,
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                    let old_short_id = ctx.current_session.short_id().to_string();
                    let old_replay_events = foreground_turn_replay_events(state);
                    let new_session = Session::default_session(ctx.working_dir.clone());
                    let new_short_id = new_session.short_id().to_string();
                    let (runtime_id, endpoint, new_session) = spawn_runtime(ctx, new_session);
                    let old_state = foreground_state_from_ui(state);
                    let slot = match ctx.bg_manager.background_current_with_replay(
                        endpoint.clone(),
                        new_session.clone(),
                        ctx.working_dir.clone(),
                        runtime_id,
                        old_state,
                        old_replay_events,
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
                        Err(
                            bg_runtime::BgError::NoRuntimeClient { .. }
                            | bg_runtime::BgError::SessionProjectionUnavailable { .. },
                        ) => unreachable!("background_current cannot return a resume error"),
                    };
                    ctx.runtime = endpoint.native;
                    ctx.foreground_runtime_id = runtime_id;
                    ctx.current_session = new_session;
                    bind_telemetry_to_session(ctx, &ctx.current_session);
                    state.on_turn_complete();
                    state.on_session_replaced();
                    // The todo panel is per-session and is NOT cleared at turn end;
                    // this fresh foreground session has no todos, so drop the prior
                    // session's list (mirrors reset_to_new_session / native SessionChanged).
                    state.active_todos = None;
                    state.pending_todo_calls.clear();
                    crate::event_loop::sync_todo_titles(state); // drop prior session's titles
                    state.approval_panel = None;
                    // One DECSET 2026 envelope around the wipe + welcome
                    // re-render so the foreground swap shows no blank frame
                    // (same anti-flicker as `/resume`). Self-contained: the
                    // arm has no early return between begin/end_sync.
                    renderer.begin_sync();
                    renderer.reset();
                    render_welcome(renderer, ctx);
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::BgBackgroundCurrent {
                            new_id: &new_short_id,
                            slot,
                            old_id: &old_short_id,
                            state: &old_state.localised(),
                        })
                        .into_owned(),
                    ));
                    renderer.flush();
                    renderer.end_sync();
                }
                bg_runtime::BgCommand::Resume(slot) => {
                    if let Err(error) = ensure_bg_foreground_switch_allowed(
                        ctx.live_binding.is_some(),
                        provider_transition_pending(ctx),
                        ctx.pending_runtime_request_id.is_some(),
                    ) {
                        renderer.render(UiLine::Error(error.into()));
                        renderer.flush();
                        return Ok(());
                    }
                    sync_bg_foreground(ctx);
                    let outcome = match ctx
                        .bg_manager
                        .resume_slot(slot, foreground_state_from_ui(state))
                    {
                        Ok(outcome) => outcome,
                        Err(bg_runtime::BgError::InvalidSlot { slot, len }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgInvalidSlot {
                                    slot,
                                    available: len,
                                })
                                .into_owned(),
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
                        Err(bg_runtime::BgError::NoRuntimeClient { .. }) => {
                            renderer.render(UiLine::Error(t(Msg::BgNoRuntimeClient).into_owned()));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(bg_runtime::BgError::SessionProjectionUnavailable {
                            error, ..
                        }) => {
                            renderer.render(UiLine::Error(format!(
                                "background session could not be loaded: {error}"
                            )));
                            renderer.flush();
                            return Ok(());
                        }
                    };
                    let endpoint = outcome.resumed_endpoint;

                    // Switching sessions: stop any active /loop so its TUI-side interval
                    // controller can't keep firing the old payload into the newly-resumed
                    // session (and clear the stale footer). ClearLoop reaches the outgoing
                    // agent before the swap below.
                    stop_active_loop(state, ctx);
                    super::commit_working_dir_projection(ctx, outcome.resumed_working_dir.clone());
                    ctx.runtime = endpoint.native;
                    ctx.foreground_runtime_id = outcome.resumed_runtime_id;
                    ctx.current_session = outcome.resumed_session;
                    bind_telemetry_to_session(ctx, &ctx.current_session);
                    apply_resumed_runtime_state(state, outcome.resumed_state);
                    crate::modals::session_picker::replay_session(
                        renderer,
                        state,
                        &ctx.current_session,
                        true,
                    );
                    schedule_resumed_runtime_replay(
                        &mut ctx.foreground_replay_events,
                        outcome.replay_events,
                    );

                    let short_id = ctx.current_session.short_id().to_string();
                    let mut msg = t(Msg::BgResumed {
                        slot,
                        short_id: &short_id,
                    })
                    .into_owned();
                    if let Some(previous_slot) = outcome.previous_foreground_slot {
                        msg.push_str(
                            &t(Msg::BgPreviousForegroundMoved {
                                slot: previous_slot,
                            })
                            .into_owned(),
                        );
                    }
                    renderer.render(UiLine::CommandOutput(msg));
                }
                bg_runtime::BgCommand::Drop(slot) => {
                    let dropped = match ctx.bg_manager.drop_slot(slot) {
                        Ok(dropped) => dropped,
                        Err(bg_runtime::BgError::InvalidSlot { slot, len }) => {
                            renderer.render(UiLine::Error(
                                t(Msg::BgInvalidSlot {
                                    slot,
                                    available: len,
                                })
                                .into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(bg_runtime::BgError::SlotLimit { .. }) => unreachable!(),
                        Err(
                            bg_runtime::BgError::NoRuntimeClient { .. }
                            | bg_runtime::BgError::SessionProjectionUnavailable { .. },
                        ) => unreachable!("drop_slot cannot return a resume error"),
                    };
                    if matches!(dropped.state, bg_runtime::RuntimeState::Running) {
                        if let Some(endpoint) = dropped.endpoint.as_ref() {
                            endpoint
                                .native
                                .dispatch(atomcode_coding::DriverCommand::Cancel)
                                .ok();
                        }
                    }
                    let short_id = dropped.session.short_id().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::BgDropped {
                            slot,
                            short_id: &short_id,
                        })
                        .into_owned(),
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
                renderer.render(UiLine::CommandOutput(t(Msg::BackgroundUsage).into_owned()));
                renderer.flush();
                return Ok(());
            }
            if !ctx.bg_manager.has_capacity() {
                renderer.render(UiLine::Error(
                    t(Msg::BgSlotLimitReached {
                        max: bg_runtime::MAX_BACKGROUND_SLOTS,
                    })
                    .into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
            let mut session = Session::default_session(ctx.working_dir.clone());
            session.name = short_task_name(task);
            let short_id = session.short_id().to_string();
            let (runtime_id, endpoint, session) = spawn_runtime(ctx, session);
            let slot = match ctx.bg_manager.push_background_runtime(
                runtime_id,
                endpoint.clone(),
                session,
                ctx.working_dir.clone(),
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
                Err(
                    bg_runtime::BgError::NoRuntimeClient { .. }
                    | bg_runtime::BgError::SessionProjectionUnavailable { .. },
                ) => unreachable!("push_background_runtime cannot return a resume error"),
            };
            let submit_result =
                endpoint
                    .native
                    .dispatch_when_ready(atomcode_coding::DriverCommand::Submit(
                        task.to_string().into(),
                    ));
            if let Err(error) =
                finalize_background_submission(&mut ctx.bg_manager, slot, submit_result)
            {
                renderer.render(UiLine::Error(format!(
                    "background task could not be started: {error}"
                )));
                renderer.flush();
                return Ok(());
            }
            ctx.bg_manager.apply_background_event(
                runtime_id,
                bg_runtime::RuntimeEventPayload::Ui(
                    crate::event_loop::ui_event::UiEvent::UserEcho(task.to_string()),
                ),
            );
            renderer.render(UiLine::CommandOutput(
                t(Msg::BgTaskStarted {
                    slot,
                    short_id: &short_id,
                })
                .into_owned(),
            ));
            renderer.flush();
        }
        "init" => {
            // LLM-driven: submit the init prompt as a normal user turn; the agent explores the
            // repo with its tools and writes/improves AGENTS.md via write_file. Replaces the old
            // static .atomcode.md generator.
            submit_agent_turn(ctx, state, atomcode_coding::INIT_PROMPT.to_string());
            renderer.render(UiLine::CommandOutput(t(Msg::InitKickoff).into_owned()));
            renderer.flush();
        }
        "mcp" => {
            let sub = arg.trim();
            match parse_mcp_subcommand(sub) {
                Some(McpSub::Login) => {
                    let server = sub.strip_prefix("login").map(str::trim).unwrap_or("");
                    if server.is_empty() {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::McpOAuthLoginUsage).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                    let configs =
                        match atomcode_capabilities::mcp::load_mcp_config(&ctx.working_dir) {
                            Ok(configs) => configs,
                            Err(e) => {
                                renderer.render(UiLine::Error(
                                    t(Msg::McpOAuthLoadConfigFailed {
                                        error: &format!("{:#}", e),
                                    })
                                    .into_owned(),
                                ));
                                renderer.flush();
                                return Ok(());
                            }
                        };
                    let Some(config) = configs.into_iter().find(|config| config.name == server)
                    else {
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
                        atomcode_capabilities::mcp::McpTransportConfig::Http {
                            auth: Some(atomcode_capabilities::mcp::McpHttpAuthConfig::OAuth(auth)),
                            ..
                        } if auth.provider.as_deref() == Some("github")
                    );
                    let result = tokio::task::block_in_place(|| {
                        atomcode_capabilities::mcp::login_mcp_oauth(
                            &config,
                            atomcode_capabilities::mcp::McpOAuthLoginOptions {
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
                        Ok(token) => {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::McpOAuthSaved {
                                    provider: &token.provider,
                                    server,
                                })
                                .into_owned(),
                            ));
                            renderer.flush();
                            return execute_slash_command_impl(
                                "mcp",
                                "reload",
                                state,
                                ctx,
                                renderer,
                                active_modal,
                                setup_pending,
                            );
                        }
                        Err(e) => renderer.render(UiLine::Error(
                            t(Msg::McpOAuthFailed {
                                error: &format!("{:#}", e),
                            })
                            .into_owned(),
                        )),
                    }
                    renderer.flush();
                    return Ok(());
                }

                Some(McpSub::Logout) => {
                    let server = sub.strip_prefix("logout").map(str::trim).unwrap_or("");
                    if server.is_empty() {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::McpOAuthLogoutUsage).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                    let token_store = atomcode_capabilities::mcp::McpTokenStore::default();
                    match token_store.load_token(server) {
                        Ok(None) => {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::McpOAuthNoToken { server }).into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Err(e) => {
                            renderer.render(UiLine::Error(
                                t(Msg::McpOAuthLogoutFailed {
                                    error: &format!("{:#}", e),
                                })
                                .into_owned(),
                            ));
                            renderer.flush();
                            return Ok(());
                        }
                        Ok(Some(_)) => {}
                    }
                    if let Err(error) = super::withdraw_mcp_tools(ctx) {
                        renderer.render(UiLine::Error(error));
                        renderer.flush();
                        return Ok(());
                    }
                    match token_store.delete_token(server) {
                        Ok(true) => {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::McpOAuthTokenRemoved { server }).into_owned(),
                            ));
                            renderer.flush();
                            return execute_slash_command_impl(
                                "mcp",
                                "reload",
                                state,
                                ctx,
                                renderer,
                                active_modal,
                                setup_pending,
                            );
                        }
                        Ok(false) => renderer.render(UiLine::CommandOutput(
                            t(Msg::McpOAuthNoToken { server }).into_owned(),
                        )),
                        Err(e) => renderer.render(UiLine::Error(
                            t(Msg::McpOAuthLogoutFailed {
                                error: &format!("{:#}", e),
                            })
                            .into_owned(),
                        )),
                    }
                    renderer.flush();
                    return Ok(());
                }

                Some(McpSub::Trust) => {
                    match atomcode_capabilities::mcp::trust::trust_project(&ctx.working_dir) {
                        Ok(()) => {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::McpProjectTrusted).into_owned(),
                            ));
                            renderer.flush();
                            // Trigger a reload so newly-allowed servers connect immediately.
                            return execute_slash_command_impl(
                                "mcp",
                                "reload",
                                state,
                                ctx,
                                renderer,
                                active_modal,
                                setup_pending,
                            );
                        }
                        Err(e) => {
                            renderer.render(UiLine::Error(format!("{:#}", e)));
                            renderer.flush();
                            return Ok(());
                        }
                    }
                }

                Some(McpSub::Untrust) => {
                    if !atomcode_capabilities::mcp::trust::is_project_trusted(&ctx.working_dir) {
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::McpProjectNotTrusted).into_owned(),
                        ));
                        renderer.flush();
                        return Ok(());
                    }
                    if let Err(error) = super::withdraw_mcp_tools(ctx) {
                        renderer.render(UiLine::Error(error));
                        renderer.flush();
                        return Ok(());
                    }
                    match atomcode_capabilities::mcp::trust::untrust_project(&ctx.working_dir) {
                        Ok(true) => {
                            renderer.render(UiLine::CommandOutput(
                                t(Msg::McpProjectUntrusted).into_owned(),
                            ));
                            renderer.flush();
                            return execute_slash_command_impl(
                                "mcp",
                                "reload",
                                state,
                                ctx,
                                renderer,
                                active_modal,
                                setup_pending,
                            );
                        }
                        Ok(false) => renderer.render(UiLine::CommandOutput(
                            t(Msg::McpProjectNotTrusted).into_owned(),
                        )),
                        Err(e) => renderer.render(UiLine::Error(format!("{:#}", e))),
                    }
                    renderer.flush();
                    return Ok(());
                }

                Some(McpSub::Reload) => {
                    // Withdraw first. Config parse and replacement prepare both read
                    // mutable security inputs and may fail; neither failure may leave
                    // the previous MCP authority mounted.
                    if let Err(error) = super::withdraw_mcp_tools(ctx) {
                        renderer.render(UiLine::Error(error));
                        renderer.flush();
                        return Ok(());
                    }
                    // Preflight: parse merged MCP config so we can show progress immediately.
                    // (Connection attempts happen in background and may take up to timeout_ms.)
                    let configs =
                        match atomcode_capabilities::mcp::load_mcp_config(&ctx.working_dir) {
                            Ok(c) => c,
                            Err(e) => {
                                renderer.render(UiLine::Error(
                                    t(Msg::McpReloadFailed {
                                        error: &format!("{:#}", e),
                                    })
                                    .into_owned(),
                                ));
                                renderer.flush();
                                return Ok(());
                            }
                        };

                    // Partition by trust so the preflight header only lists servers that will
                    // actually be attempted (project-source servers from untrusted projects are
                    // withheld and should not appear in the "connecting to:" list).
                    let partition = atomcode_capabilities::mcp::trust::partition_by_trust(
                        configs.clone(),
                        &ctx.working_dir,
                    );
                    let connecting = &partition.allowed;

                    let mut header = t(Msg::McpReloading {
                        count: configs.len(),
                    })
                    .into_owned();

                    if !connecting.is_empty() {
                        header.push_str(&t(Msg::McpConnecting));
                        for c in connecting {
                            header.push_str(&t(Msg::McpConnectingServer { name: &c.name }));
                        }
                    } else if !configs.is_empty() {
                        // All servers are blocked (untrusted project); nothing will connect.
                        header.push_str(&t(Msg::McpNoServersConfigured));
                    } else {
                        header.push_str(&t(Msg::McpNoServersConfigured));
                    }
                    renderer.render(UiLine::CommandOutput(header));
                    renderer.flush();

                    // Every MCP mutation converges here. CodingRuntime owns the
                    // model-facing catalog, including the empty-config case where
                    // all previously mounted MCP tools must be removed.
                    if let Err(error) = super::request_capability_reload(ctx) {
                        renderer.render(UiLine::Error(error));
                        renderer.flush();
                        return Ok(());
                    }
                    ctx.pending_mcp_reload_server_count = Some(configs.len());
                    return Ok(());
                }

                Some(McpSub::Tools) => {
                    // `/mcp tools <server>`: list remote tool names for a connected server.
                    // This is intentionally separate from a global `/tools` so we keep the surface minimal.
                    let server = sub.strip_prefix("tools").map(str::trim).unwrap_or("");
                    if server.is_empty() {
                        renderer.render(UiLine::CommandOutput(t(Msg::McpToolsUsage).into_owned()));
                        renderer.flush();
                        return Ok(());
                    }
                    let result = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current()
                            .block_on(ctx.runtime.mcp_tools(server.to_string()))
                    });
                    match result {
                        Ok(snapshot) => {
                            let mut message = String::from("tools:\n");
                            if snapshot.tools.is_empty() {
                                match snapshot.status {
                                    Some(status) => {
                                        message.push_str(&format!("  (none — {status})\n"));
                                    }
                                    None => message.push_str("  (none — server not configured)\n"),
                                }
                            } else {
                                for tool in snapshot.tools {
                                    message.push_str(&format!("  - {tool}\n"));
                                }
                            }
                            renderer.render(UiLine::CommandOutput(message.trim_end().to_string()));
                        }
                        Err(error) => renderer.render(UiLine::Error(error.to_string())),
                    }
                    renderer.flush();
                    return Ok(());
                }

                None => { /* fall through to default status display below */ }
            }

            // Default: show status.
            let status = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(ctx.runtime.mcp_status())
            });
            match status {
                Ok(status) if status.servers.is_empty() => {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::McpNoServersConfigured).into_owned(),
                    ));
                }
                Ok(status) => {
                    let mut txt = t(Msg::McpServersHeader).into_owned();
                    for (name, server_status) in status.servers {
                        txt.push_str(&format!("    {}  {}\n", name, server_status));
                    }
                    renderer.render(UiLine::CommandOutput(txt));
                }
                Err(error) => renderer.render(UiLine::Error(error.to_string())),
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
            let provider_name = ctx.config.effective_model_selection().unwrap_or_default();
            let provider = ctx.config.provider_config_for_selection(&provider_name);
            match provider {
                None => {
                    renderer.render(UiLine::Error(t(Msg::CmdNoActiveProvider).into_owned()));
                    renderer.flush();
                }
                Some(p) => {
                    if sub.is_empty() {
                        // Show current status
                        let enabled = p.thinking_enabled.unwrap_or(false);
                        let budget = p.thinking_budget.unwrap_or(10_000);
                        let status = if enabled { "enabled" } else { "disabled" };
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::ThinkStatus {
                                status,
                                budget,
                                provider: &provider_name,
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    } else if sub == "on" {
                        let budget = p.thinking_budget.unwrap_or(10_000);
                        let mut desired = ctx.config.clone();
                        desired.update_selection_reasoning(&provider_name, |r| {
                            *r.thinking_enabled = Some(true)
                        });
                        save_and_reload(
                            ctx,
                            desired,
                            renderer,
                            t(Msg::ThinkEnabled { budget }).into_owned(),
                            false,
                        );
                    } else if sub == "off" {
                        let mut desired = ctx.config.clone();
                        desired.update_selection_reasoning(&provider_name, |r| {
                            *r.thinking_enabled = Some(false)
                        });
                        save_and_reload(
                            ctx,
                            desired,
                            renderer,
                            t(Msg::ThinkDisabled).into_owned(),
                            false,
                        );
                    } else if let Some(rest) = sub.strip_prefix("budget") {
                        let num_str = rest.trim();
                        match num_str.parse::<u32>() {
                            Ok(n) if n >= 1024 => {
                                let mut desired = ctx.config.clone();
                                desired.update_selection_reasoning(&provider_name, |r| {
                                    *r.thinking_budget = Some(n)
                                });
                                save_and_reload(
                                    ctx,
                                    desired,
                                    renderer,
                                    t(Msg::ThinkBudgetSet { n }).into_owned(),
                                    false,
                                );
                            }
                            Ok(n) => {
                                renderer.render(UiLine::Error(
                                    t(Msg::ThinkBudgetTooSmall { n }).into_owned(),
                                ));
                                renderer.flush();
                            }
                            Err(_) => {
                                renderer
                                    .render(UiLine::Error(t(Msg::ThinkBudgetUsage).into_owned()));

                                renderer.flush();
                            }
                        }
                    } else {
                        renderer.render(UiLine::CommandOutput(t(Msg::ThinkUsage).into_owned()));
                        renderer.flush();
                    }
                }
            }
        }
        "effort" => {
            let sub = arg.trim().to_ascii_lowercase();
            let provider_name = ctx.config.effective_model_selection().unwrap_or_default();
            let applicable = crate::event_loop::reasoning_effort_applicable_on_provider(ctx);
            if !applicable {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::ReasoningEffortNoEffect).into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
            let provider = ctx.config.provider_config_for_selection(&provider_name);
            match provider {
                None => {
                    renderer.render(UiLine::Error(t(Msg::CmdNoActiveProvider).into_owned()));
                    renderer.flush();
                }
                Some(p) => {
                    if sub.is_empty() {
                        // Show current status
                        let current = p.reasoning_effort.as_deref().unwrap_or("off (API default)");
                        renderer.render(UiLine::CommandOutput(format!(
                            "  Current reasoning effort: {current}\n  Usage: /effort high | max | off\n  Shortcut: Ctrl+T\n"
                        )));
                        renderer.flush();
                    } else if sub == "high" || sub == "max" {
                        let mut desired = ctx.config.clone();
                        desired.update_selection_reasoning(&provider_name, |r| {
                            *r.reasoning_effort = Some(sub.to_string())
                        });
                        crate::event_loop::save_and_reload(
                            ctx,
                            desired,
                            renderer,
                            format!("  ○ Reasoning effort set to: {sub}\n"),
                            false,
                        );
                    } else if sub == "off" {
                        let mut desired = ctx.config.clone();
                        desired.update_selection_reasoning(&provider_name, |r| {
                            *r.reasoning_effort = None
                        });
                        crate::event_loop::save_and_reload(
                            ctx,
                            desired,
                            renderer,
                            "  ○ Reasoning effort: default (API auto)\n".to_string(),
                            false,
                        );
                    } else {
                        renderer.render(UiLine::CommandOutput(
                            "  Usage: /effort high | max | off\n  Shortcut: Ctrl+T\n".into(),
                        ));
                        renderer.flush();
                    }
                }
            }
        }
        "goal" => {
            // Sub-commands aligned with Claude Code's /goal (v2.1.139+):
            //   /goal <condition>             → set a new goal
            //   /goal                         → show status (or hint if none)
            //   /goal status                  → explicit status (same)
            //   /goal clear|stop|off|reset|none|cancel  → halt the active goal
            //   /goal help|?|-h|--help        → usage
            //
            // CC has no `--max-rounds` flag and no wall-clock cap. Users
            // express budgets in the condition text instead (e.g. "or stop
            // after 20 turns"). Esc / Ctrl+C also halts at any time.
            let trimmed = arg.trim();
            let (head, _rest) = trimmed
                .split_once(char::is_whitespace)
                .map(|(h, r)| (h, r.trim()))
                .unwrap_or((trimmed, ""));
            match head {
                "arm" => {
                    // Mobile App selected Goal mode but has not supplied the
                    // condition yet. The next idle TUI submit becomes the
                    // condition and is executed through this same command arm.
                    state.goal_armed = true;
                    renderer.flush();
                }
                "" | "status" => {
                    if let Some(ref cond) = state.goal_condition {
                        // Display 1-based, consistent with the footer goal row.
                        let round = state.goal_round + 1;
                        let elapsed = state
                            .goal_started_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let mins = elapsed / 60;
                        let secs = elapsed % 60;
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::GoalStatus {
                                condition: cond.as_str(),
                                round,
                                mins,
                                secs,
                            })
                            .into_owned(),
                        ));
                    } else {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::GoalNoActive).into_owned(),
                        ));
                    }
                    renderer.flush();
                }
                "clear" | "stop" | "off" | "reset" | "none" | "cancel" => {
                    ctx.runtime
                        .dispatch(atomcode_coding::DriverCommand::StopGoal)
                        .ok();
                    state.goal_condition = None;
                    state.goal_round = 0;
                    state.goal_started_at = None;
                    state.goal_armed = false;
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::GoalCleared).into_owned(),
                    ));
                    renderer.flush();
                }
                "help" | "?" | "-h" | "--help" => {
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::GoalHelp).into_owned(),
                    ));
                    renderer.flush();
                }
                _ => {
                    // Treat the entire trimmed input as the condition.
                    // (Empty input is unreachable here — `head` would be ""
                    // and the `"" | "status"` arm above would have matched.)
                    let condition = trimmed.to_owned();
                    if ctx
                        .runtime
                        .dispatch(atomcode_coding::DriverCommand::StartGoal(condition.clone()))
                        .is_err()
                    {
                        renderer.render(UiLine::Error(t(Msg::CmdProviderUnavailable).into_owned()));
                        renderer.flush();
                        return Ok(());
                    }
                    state.goal_condition = Some(condition.clone());
                    state.goal_armed = false;
                    state.goal_round = 0;
                    state.goal_started_at = Some(std::time::Instant::now());
                    if submit_agent_text(ctx, condition) {
                        state.on_submit();
                    }
                }
            }
        }
        "loop" => {
            use crate::event_loop::loop_parse::{parse_loop_arg, LoopArg};
            match parse_loop_arg(arg) {
                LoopArg::Status => {
                    if let Some(ref label) = state.loop_label.clone() {
                        let secs = state
                            .loop_started_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let mins = secs / 60;
                        let secs_rem = secs % 60;
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::LoopStatus {
                                label: label.as_str(),
                                round: (state.loop_round + 1) as u32,
                                mins,
                                secs: secs_rem,
                            })
                            .into_owned(),
                        ));
                    } else {
                        renderer.render(UiLine::CommandOutput(
                            crate::i18n::t(crate::i18n::Msg::LoopNoActive).into_owned(),
                        ));
                    }
                    renderer.flush();
                }
                LoopArg::Stop => {
                    stop_active_loop(state, ctx);
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::LoopCleared).into_owned(),
                    ));
                    renderer.flush();
                }
                LoopArg::SelfPaced { prompt } => {
                    // Replace any existing loop (both self-paced core and
                    // fixed-interval TUI controller) before setting a new one.
                    stop_active_loop(state, ctx);
                    if ctx
                        .runtime
                        .dispatch(atomcode_coding::DriverCommand::StartLoop(prompt.clone()))
                        .is_err()
                    {
                        renderer.render(UiLine::Error(t(Msg::CmdProviderUnavailable).into_owned()));
                        renderer.flush();
                        return Ok(());
                    }
                    state.loop_label = Some(prompt.clone());
                    state.loop_round = 0;
                    state.loop_started_at = Some(std::time::Instant::now());
                    if submit_agent_text(ctx, prompt) {
                        state.on_submit();
                    }
                    // Non-silent: /loop is live-only (persistence deferred) — tell the
                    // user it won't come back after a restart/resume.
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::LoopNoPersistHint).into_owned(),
                    ));
                }
                LoopArg::Interval { secs, payload } => {
                    // Fixed-interval mode: stop any currently running loop
                    // (self-paced or fixed-interval) first, then install a
                    // fresh wall-clock LoopController and fire the first
                    // iteration now. The TUI event loop's deadline arm +
                    // TurnComplete hook re-fire it on schedule while the agent
                    // is idle (see run_loop / handle_loop_decision).
                    stop_active_loop(state, ctx);
                    start_interval_loop(
                        state,
                        ctx,
                        renderer,
                        secs,
                        payload,
                        active_modal,
                        setup_pending,
                    );
                    // Non-silent: /loop is live-only (persistence deferred) — tell the
                    // user it won't come back after a restart/resume.
                    renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::LoopNoPersistHint).into_owned(),
                    ));
                    renderer.flush();
                }
                LoopArg::Error(msg) => {
                    renderer.render(UiLine::Error(msg));
                    renderer.flush();
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
                    renderer.render(UiLine::CommandOutput(t(Msg::SkillsNone).into_owned()));
                } else {
                    renderer.render(UiLine::CommandOutput(format!(
                        "{}{}\n",
                        t(Msg::SkillsAvailable),
                        lines.join("\n")
                    )));
                }
                renderer.flush();
            } else {
                // 贪婪多 skill 解析：前缀是一串已知 skill 名，其余是任务描述，
                // 任务描述会传给每个 skill（保留 $ARGUMENTS 占位符语义）。单个
                // skill（无第二个 skill 词）解析结果与旧 splitn(2) 一致，零回归。
                // Returns the skill's canonical identity (`s.name`, the unique
                // normalized registry key) so `split_skill_names` dedups by
                // identity — two spellings of the same skill inject it once.
                let resolve = |name: &str| {
                    ctx.skill_registry.read().ok().and_then(|r| {
                        r.get(name)
                            .filter(|s| s.user_invocable)
                            .map(|s| s.name.clone())
                    })
                };
                let (skills, skill_args) = split_skill_names(arg_trim, resolve);
                if skills.is_empty() {
                    // 首词不是 skill —— 沿用旧的 unknown 报错，指名第一个词。
                    let first = arg_trim.split_whitespace().next().unwrap_or("");
                    renderer.render(UiLine::Error(
                        t(Msg::SkillUnknown { name: first }).into_owned(),
                    ));
                    renderer.flush();
                } else {
                    // 按顺序展开每个 skill；expand_skill 可能因竞态返回 None。
                    let blocks: Vec<String> = skills
                        .iter()
                        .filter_map(|name| expand_skill(ctx, name.as_str(), &skill_args))
                        .collect();
                    if blocks.is_empty() {
                        renderer.render(UiLine::Error(
                            t(Msg::SkillUnknown {
                                name: skills[0].as_str(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                    } else {
                        // 回显已加载 skill：第二个及以后的 skill 名若打错字会静默
                        // 落进任务描述，这行让用户一眼看出"只加载了 N 个"。
                        let names = skills.join(" · ");
                        renderer.render(UiLine::CommandOutput(
                            t(Msg::SkillsLoaded {
                                names: names.as_str(),
                            })
                            .into_owned(),
                        ));
                        renderer.flush();
                        let rendered = blocks.join("\n\n---\n\n");
                        submit_agent_turn(ctx, state, rendered);
                    }
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
                    *setup_pending = true;
                    submit_agent_turn(ctx, state, rendered);
                } else {
                    renderer.render(UiLine::Error(t(Msg::CmdSetupSkillMissing).into_owned()));
                    renderer.flush();
                }
            } else {
                // First run: install seeds, reload, then invoke.
                renderer.render(UiLine::CommandOutput(t(Msg::CmdSetupRunning).into_owned()));
                renderer.flush();

                let project_root = ctx.working_dir.clone();
                let opts = atomcode_capabilities::setup::RunOptions::new(project_root);

                // `setup::run` is synchronous (file I/O only). Run it on the
                // current thread via `block_in_place` to avoid blocking the
                // tokio runtime — no `block_on` needed since it's not async.
                let result =
                    tokio::task::block_in_place(|| atomcode_capabilities::setup::run(opts));

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
                            t(Msg::CmdSetupSkillsReloaded {
                                count: skills_loaded,
                            })
                            .into_owned(),
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
                            *setup_pending = true;
                            submit_agent_turn(ctx, state, rendered);
                        } else {
                            renderer
                                .render(UiLine::Error(t(Msg::CmdSetupSkillMissing).into_owned()));
                            renderer.flush();
                        }
                    }
                    Err(e) => {
                        renderer.render(UiLine::Error(
                            t(Msg::CmdSetupError {
                                error: &e.to_string(),
                            })
                            .into_owned(),
                        ));
                    }
                }
                renderer.flush();
            }
        }
        "todo" => {
            // `/todo` derives + prints the current list. Two deterministic
            // subcommands (first word, case-insensitive) mutate it without
            // waiting on the model, both via the kernel-reseed path so the next
            // turn's TodoHook reflects the change:
            //   `/todo clear`        — wipe the list (stale/cancelled tasks stop reappearing)
            //   `/todo add <text>`   — append one pending task at the end
            // Then the list is re-printed as confirmation.
            let (kw, rest) = match arg.trim().split_once(char::is_whitespace) {
                Some((k, r)) => (k, r.trim()),
                None => (arg.trim(), ""),
            };
            let is_add = kw.eq_ignore_ascii_case("add");
            if is_add && rest.is_empty() {
                // `/todo add` with no text → usage hint, no mutation, no reprint.
                renderer.render(UiLine::CommandOutput(t(Msg::TodoAddUsage).into_owned()));
                renderer.flush();
            } else {
                if is_add {
                    add_todo(ctx, state, rest);
                } else if kw.eq_ignore_ascii_case("clear") && state.active_todos.is_some() {
                    // Only reseed when there's something to clear, so a no-op
                    // doesn't pollute the transcript with an empty-todowrite pair.
                    clear_todos(ctx, state);
                }
                // Re-print the (possibly mutated) list as confirmation — shared by
                // add-success, clear, and a bare `/todo`.
                let out =
                    format_todo_command(&ctx.current_session.messages, ctx.caps.unicode_symbols);
                renderer.render(UiLine::CommandOutput(out));
                renderer.flush();
            }
        }
        other => {
            // Before reporting "unknown", check user-defined custom commands,
            // then user-invocable skills (loaded from .claude/skills,
            // .atomcode/skills, etc.). Both expand to a prompt and dispatch
            // as a regular user message.
            match decide_custom_command(&ctx.custom_commands, other, arg) {
                CustomDispatch::Reject => {
                    render_custom_command_error(renderer, &CustomDispatch::Reject, other);
                }
                CustomDispatch::Submit(rendered) => {
                    submit_agent_turn(ctx, state, rendered);
                }
                CustomDispatch::NotFound => {
                    if let Some(rendered) = expand_skill(ctx, other, arg) {
                        submit_agent_turn(ctx, state, rendered);
                    } else {
                        // Unknown command — emit failure telemetry
                        let available_commands: Vec<&str> =
                            crate::commands::CommandRegistry::builtin()
                                .all()
                                .iter()
                                .map(|command| command.name)
                                .collect();
                        ctx.telemetry.track(atomcode_telemetry::Event::UseCommand {
                            type_: other.to_string(),
                            success: Some(false),
                            error_kind: Some(atomcode_telemetry::UseCommandErrorKind::NotFound),
                            error_data: Some(
                                serde_json::json!({
                                    "command": other,
                                    "duration_ms": 0,
                                    "message": format!("Unknown command: {}", other),
                                    "reason": "用户输入了不存在的斜杠命令",
                                    "resolution": "使用 /help 查看所有可用命令",
                                    "available_commands": available_commands,
                                })
                                .to_string(),
                            ),
                        });
                        render_custom_command_error(renderer, &CustomDispatch::NotFound, other);
                    }
                }
            }
        }
    }
    Ok(())
}

/// 贪婪切分 `/skills` 参数：从左到右扫 whitespace 分词，`resolve(token)` 返回该
/// token 对应 skill 的**规范身份**（`Some(canonical)`）时收入列表，否则停止。去重
/// 按规范身份而非原始拼写——两个拼写不同但解析到同一 skill 的 token（大小写、后缀
/// 简写等）只注入一次。列表里存用户原始拼写（回显更友好），展开时 `expand_skill`
/// 会再归一化。遇到第一个非 skill 的 token，它及其之后的内容（按原串偏移，保留原
/// 空白）作为任务描述返回。单个 skill（后面无第二个 skill 词）等价于旧 `splitn(2)`。
fn split_skill_names(arg: &str, resolve: impl Fn(&str) -> Option<String>) -> (Vec<String>, String) {
    let mut skills: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut rest = arg.trim_start();
    loop {
        let token_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let token = &rest[..token_end];
        if token.is_empty() {
            break;
        }
        let Some(canonical) = resolve(token) else {
            break;
        };
        if !seen.iter().any(|c| c == &canonical) {
            seen.push(canonical);
            skills.push(token.to_string());
        }
        rest = rest[token_end..].trim_start();
    }
    (skills, rest.to_string())
}

/// Decision returned by [`decide_custom_command`] for the `other` arm of
/// [`execute_slash_command_impl`]. Separating the pure decision from its
/// side effects (rendering an error line, submitting an agent turn) lets
/// the dispatch rule — `Required` + empty arg ⇒ reject — be unit-tested
/// without constructing a full `LoopCtx`.
#[derive(Debug)]
pub(super) enum CustomDispatch {
    /// `args_requirement == Required` and the user supplied no argument:
    /// render `CmdCustomArgRequired` and do NOT submit a turn.
    Reject,
    /// Custom command resolved; expand its template with `arg` and send
    /// the rendered text as a user message.
    Submit(String),
    /// No custom command matched this name; fall through to the
    /// user-invocable skill lookup / unknown-command path.
    NotFound,
}

/// Render the error line for a [`CustomDispatch::Reject`] (Required
/// command, no argument supplied) or [`CustomDispatch::NotFound`] (no
/// custom command matched and no user-invocable skill matched either)
/// outcome of the `other` arm.
///
/// Pure side effect on `renderer` only — does NOT call
/// `submit_agent_turn`, which is the whole point of both error paths:
///   - `Reject`   ⇒ user typed e.g. `/myreview` with no argument; surface
///     `Msg::CmdCustomArgRequired` and leave the conversation untouched.
///   - `NotFound` ⇒ user typed e.g. `/foo` that matches nothing; surface
///     `Msg::CmdUnknownCommand` (telemetry is tracked by the caller).
///   - `Submit`   ⇒ not an error; the caller forwards the rendered template
///     to `submit_agent_turn`, so this helper is a no-op there.
///
/// Extracted from the `other` arm so the reject-vs-submit render boundary
/// is unit-testable without constructing a full `LoopCtx`.
pub(super) fn render_custom_command_error(
    renderer: &mut dyn Renderer,
    dispatch: &CustomDispatch,
    name: &str,
) {
    match dispatch {
        CustomDispatch::Reject => {
            renderer.render(UiLine::Error(
                t(Msg::CmdCustomArgRequired { name }).into_owned(),
            ));
            renderer.flush();
        }
        CustomDispatch::NotFound => {
            renderer.render(UiLine::Error(
                t(Msg::CmdUnknownCommand { name }).into_owned(),
            ));
            renderer.flush();
        }
        // Submit is not an error — handled by the caller via submit_agent_turn.
        CustomDispatch::Submit(_) => {}
    }
}

/// Resolve `name` against the custom command registry and decide what the
/// dispatcher should do. Pure: touches neither `LoopCtx` nor the renderer,
/// so the reject-vs-submit boundary is testable in isolation.
///
/// - `Required` + empty/whitespace-only `arg` → `Reject`
/// - otherwise resolved command → `Submit(render(arg))`
/// - no match → `NotFound`
pub(super) fn decide_custom_command(
    registry: &crate::custom_commands::CustomCommandRegistry,
    name: &str,
    arg: &str,
) -> CustomDispatch {
    match registry.resolve(name) {
        Some(cmd) => {
            if cmd.args_requirement == ArgsRequirement::Required && arg.trim().is_empty() {
                CustomDispatch::Reject
            } else {
                CustomDispatch::Submit(cmd.render(arg))
            }
        }
        None => CustomDispatch::NotFound,
    }
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
    Some(skill.expand_for_injection(arg, ctx.current_session.id.as_str()))
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
                    ok(
                        renderer,
                        t(Msg::PluginMarketplaceCloning { url: &url }).into_owned(),
                    );
                    tokio::task::spawn_blocking(move || {
                        let ev =
                            match atomcode_capabilities::plugin::marketplace::add_marketplace(&url)
                            {
                                Ok(info) => {
                                    atomcode_capabilities::plugin::PluginJobEvent::MarketplaceAdded(
                                        info,
                                    )
                                }
                                Err(e) => atomcode_capabilities::plugin::PluginJobEvent::Failed {
                                    op: "add marketplace".into(),
                                    msg: format!("{:#}", e),
                                },
                            };
                        let _ = tx.send(ev);
                    });
                }
                "remove" => {
                    match atomcode_capabilities::plugin::marketplace::remove_marketplace(arg) {
                        Ok(()) => {
                            super::reload_plugins(ctx);
                            ok(
                                renderer,
                                t(Msg::PluginMarketplaceRemoved { name: arg }).into_owned(),
                            );
                        }
                        Err(e) => err(
                            renderer,
                            t(Msg::PluginMarketplaceRemoveFailed {
                                error: &e.to_string(),
                            })
                            .into_owned(),
                        ),
                    }
                }
                "update" => {
                    let name = arg.to_string();
                    let tx = ctx.plugin_job_tx.clone();
                    ok(
                        renderer,
                        t(Msg::PluginMarketplaceUpdating { name: &name }).into_owned(),
                    );
                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_capabilities::plugin::marketplace::update_marketplace(&name)
                        {
                            Ok(info) => {
                                atomcode_capabilities::plugin::PluginJobEvent::MarketplaceUpdated(info)
                            }
                            Err(e) => atomcode_capabilities::plugin::PluginJobEvent::Failed {
                                op: "update marketplace".into(),
                                msg: format!("{:#}", e),
                            },
                        };
                        let _ = tx.send(ev);
                    });
                }
                "list" => match atomcode_capabilities::plugin::marketplace::list_marketplaces() {
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
                        renderer
                            .render(UiLine::CommandOutput(format!("  {}\n", lines.join("\n  "))));
                        renderer.flush();
                    }
                    Err(e) => err(
                        renderer,
                        t(Msg::PluginMarketplaceListFailed {
                            error: &e.to_string(),
                        })
                        .into_owned(),
                    ),
                },
                _ => err(renderer, t(Msg::PluginMarketplaceUsage).into_owned()),
            }
        }
        "install" => {
            // Parse: /plugin install <plugin>@<marketplace> [--scope user|project|local]
            let rest = parts.next().unwrap_or("").trim();
            let scope_arg = parts.next().unwrap_or("").trim();
            let scope = parse_scope_arg(scope_arg);
            match parse_plugin_arg(rest) {
                Some(PluginArg::Qualified {
                    plugin,
                    marketplace: mp,
                }) => {
                    // Explicit plugin@marketplace — install directly.
                    let tx = ctx.plugin_job_tx.clone();
                    ok(
                        renderer,
                        t(Msg::PluginInstalling {
                            plugin: &plugin,
                            marketplace: &mp,
                        })
                        .into_owned(),
                    );
                    tokio::task::spawn_blocking(move || {
                        let ev = match atomcode_capabilities::plugin::installer::install(&plugin, &mp, scope) {
                            Ok(info) => atomcode_capabilities::plugin::PluginJobEvent::PluginInstalled(info),
                            Err(e) => {
                                if let Some(_aie) = e.downcast_ref::<atomcode_capabilities::plugin::installer::AlreadyInstalledError>() {
                                    atomcode_capabilities::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                        id: _aie.id.clone(),
                                    }
                                } else {
                                    atomcode_capabilities::plugin::PluginJobEvent::Failed {
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
                    match atomcode_capabilities::plugin::installer::resolve_plugin_marketplace(
                        &plugin,
                    ) {
                        Ok(matches) if matches.len() == 1 => {
                            let m = &matches[0];
                            let mp = m.marketplace.clone();
                            let resolved_plugin = m.plugin.clone();
                            let tx = ctx.plugin_job_tx.clone();
                            ok(
                                renderer,
                                t(Msg::PluginInstallingByName { plugin: &plugin }).into_owned(),
                            );
                            tokio::task::spawn_blocking(move || {
                                let ev = match atomcode_capabilities::plugin::installer::install(&resolved_plugin, &mp, scope) {
                                    Ok(info) => atomcode_capabilities::plugin::PluginJobEvent::PluginInstalled(info),
                                    Err(e) => {
                                        if let Some(_aie) = e.downcast_ref::<atomcode_capabilities::plugin::installer::AlreadyInstalledError>() {
                                            atomcode_capabilities::plugin::PluginJobEvent::PluginAlreadyInstalled {
                                                id: _aie.id.clone(),
                                            }
                                        } else {
                                            atomcode_capabilities::plugin::PluginJobEvent::Failed {
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
                            let mut msg =
                                t(Msg::PluginInstallAmbiguous { plugin: &plugin }).into_owned();
                            for m in &matches {
                                msg.push_str(&format!(
                                    "  /plugin install {}@{}\n",
                                    m.plugin, m.marketplace
                                ));
                            }
                            err(renderer, msg);
                        }
                        _ => {
                            ok(
                                renderer,
                                t(Msg::PluginInstallNotFound { plugin: &plugin }).into_owned(),
                            );
                        }
                    }
                }
                None => err(renderer, t(Msg::PluginInstallUsage).into_owned()),
            }
        }
        "uninstall" => match parse_plugin_arg(parts.next().unwrap_or("").trim()) {
            Some(PluginArg::Qualified {
                plugin,
                marketplace: mp,
            }) => {
                match atomcode_capabilities::plugin::installer::uninstall(
                    &plugin,
                    &mp,
                    atomcode_capabilities::plugin::InstallScope::User,
                ) {
                    Ok(()) => {
                        super::reload_plugins(ctx);
                        ok(
                            renderer,
                            t(Msg::PluginUninstalled {
                                plugin: &plugin,
                                marketplace: &mp,
                            })
                            .into_owned(),
                        );
                    }
                    Err(e) => err(
                        renderer,
                        t(Msg::PluginUninstallFailed {
                            error: &e.to_string(),
                        })
                        .into_owned(),
                    ),
                }
            }
            Some(PluginArg::Bare { plugin }) => {
                // Look up which installed plugins match this name.
                let installed =
                    atomcode_capabilities::plugin::installer::list_installed().unwrap_or_default();
                let matches: Vec<_> = installed
                    .into_iter()
                    .filter(|p| {
                        p.plugin == plugin
                            || p.plugin
                                == atomcode_capabilities::plugin::marketplace::sanitize_name(
                                    &plugin,
                                )
                    })
                    .collect();
                match matches.len() {
                    0 => ok(
                        renderer,
                        t(Msg::PluginUninstallNotFound { plugin: &plugin }).into_owned(),
                    ),
                    1 => {
                        let p = &matches[0];
                        let (plug, mp, scope) =
                            (p.plugin.clone(), p.marketplace.clone(), p.scope.clone());
                        match atomcode_capabilities::plugin::installer::uninstall(&plug, &mp, scope)
                        {
                            Ok(()) => {
                                super::reload_plugins(ctx);
                                ok(
                                    renderer,
                                    t(Msg::PluginUninstalled {
                                        plugin: &plug,
                                        marketplace: &mp,
                                    })
                                    .into_owned(),
                                );
                            }
                            Err(e) => err(
                                renderer,
                                t(Msg::PluginUninstallFailed {
                                    error: &e.to_string(),
                                })
                                .into_owned(),
                            ),
                        }
                    }
                    _ => {
                        let mut msg =
                            t(Msg::PluginUninstallAmbiguous { plugin: &plugin }).into_owned();
                        for p in &matches {
                            msg.push_str(&format!(
                                "  /plugin uninstall {}@{}\n",
                                p.plugin, p.marketplace
                            ));
                        }
                        err(renderer, msg);
                    }
                }
            }
            None => err(renderer, t(Msg::PluginUninstallUsage).into_owned()),
        },
        "list" => match atomcode_capabilities::plugin::installer::list_installed() {
            Ok(items) if items.is_empty() => {
                ok(renderer, t(Msg::PluginNoInstalled).into_owned());
            }
            Ok(items) => {
                let mut lines = vec![t(Msg::PluginInstalledHeader).into_owned()];
                for p in items {
                    lines.push(format!(
                        "  {}@{}  {}",
                        p.plugin, p.marketplace, p.plugin_dir
                    ));
                }
                renderer.render(UiLine::CommandOutput(format!("  {}\n", lines.join("\n  "))));
                renderer.flush();
            }
            Err(e) => err(
                renderer,
                t(Msg::PluginListFailed {
                    error: &e.to_string(),
                })
                .into_owned(),
            ),
        },
        "reload" => {
            let (skills_loaded, warnings) = super::reload_plugins(ctx);
            let warn_count = warnings.len();
            ok(
                renderer,
                t(Msg::PluginReloadDone {
                    skills: skills_loaded,
                    warnings: warn_count,
                })
                .into_owned(),
            );
            if !warnings.is_empty() {
                for w in &warnings {
                    err(renderer, w.clone());
                }
            }
        }
        _ => err(renderer, t(Msg::PluginUsage).into_owned()),
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
fn parse_scope_arg(s: &str) -> atomcode_capabilities::plugin::InstallScope {
    // Accept both `--scope user` and bare `user`.
    let val = s.strip_prefix("--scope=").unwrap_or(s).trim();
    match val.to_lowercase().as_str() {
        "project" => atomcode_capabilities::plugin::InstallScope::Project,
        "local" => atomcode_capabilities::plugin::InstallScope::Local,
        _ => atomcode_capabilities::plugin::InstallScope::User,
    }
}

/// Handle `/worktree` subcommands: create, list, done, cleanup.
fn handle_worktree(arg: &str, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> Result<()> {
    use crate::git::worktree::WorktreeManager;

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
                        t(Msg::WorktreeCreateFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            match mgr.create(branch, &base) {
                Ok(wt) => {
                    let path_str = wt.path.display().to_string();
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCreated {
                            branch: &wt.branch,
                            base: &wt.base_branch,
                            path: &path_str,
                        })
                        .into_owned(),
                    ));
                    let original = ctx.working_dir.clone();
                    match apply_cd_with_effect(
                        ctx,
                        wt.path.clone(),
                        crate::event_loop::SessionTransitionEffect::EnterWorktree {
                            original_dir: original,
                        },
                    ) {
                        Ok(_) => {}
                        Err(error) => renderer.render(UiLine::Error(error)),
                    }
                }
                Err(e) => {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeCreateFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
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
                        t(Msg::WorktreeListFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
                    ));
                    renderer.flush();
                    return Ok(());
                }
            };
            match mgr.list() {
                Ok(worktrees) => {
                    if worktrees.is_empty() {
                        renderer
                            .render(UiLine::CommandOutput(t(Msg::WorktreeNoActive).into_owned()));
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
                        t(Msg::WorktreeListFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
                    ));
                }
            }
            renderer.flush();
        }
        Some("done") => {
            if let Some(original) = ctx.worktree_original_dir.clone() {
                let current_branch = detect_current_branch(&ctx.working_dir);
                match apply_cd_with_effect(
                    ctx,
                    original.clone(),
                    crate::event_loop::SessionTransitionEffect::LeaveWorktree {
                        original_dir: original,
                        branch: current_branch,
                    },
                ) {
                    // Silent on success (fast transition); status only via guards.
                    Ok(_) => {}
                    Err(error) => renderer.render(UiLine::Error(error)),
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
                        t(Msg::WorktreeCleanupFailed {
                            error: &format!("{:#}", e),
                        })
                        .into_owned(),
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
            if removing_current {
                let target = ctx
                    .worktree_original_dir
                    .clone()
                    .unwrap_or_else(|| mgr.repo_root().to_path_buf());
                match apply_cd_with_effect(
                    ctx,
                    target.clone(),
                    crate::event_loop::SessionTransitionEffect::CleanupCurrentWorktree {
                        manager_dir: mgr.repo_root().to_path_buf(),
                        target_dir: target,
                        branch: branch.to_string(),
                        force,
                    },
                ) {
                    // Silent on success (fast transition); status only via guards.
                    Ok(_) => {}
                    Err(error) => renderer.render(UiLine::Error(error)),
                }
                renderer.flush();
                return Ok(());
            }
            match mgr.remove(branch, force) {
                Ok(()) => {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCleaned { branch }).into_owned(),
                    ));
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
            renderer.render(UiLine::CommandOutput(t(Msg::WorktreeUsage).into_owned()));
            renderer.flush();
        }
    }
    Ok(())
}

pub(crate) fn complete_session_transition_effect(
    effect: crate::event_loop::SessionTransitionEffect,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) {
    use crate::event_loop::SessionTransitionEffect;
    use crate::git::worktree::WorktreeManager;

    effect.commit_marker(&mut ctx.worktree_original_dir);
    match effect {
        SessionTransitionEffect::None | SessionTransitionEffect::EnterWorktree { .. } => {}
        SessionTransitionEffect::CdCommand { echo } => {
            renderer.render(UiLine::User(echo));
            let path = ctx.working_dir.display().to_string();
            renderer.render(UiLine::CommandOutput(
                t(Msg::DirChanged { path: &path }).into_owned(),
            ));
            renderer.flush();
        }
        SessionTransitionEffect::LeaveWorktree {
            original_dir,
            branch,
        } => {
            let path = original_dir.display().to_string();
            renderer.render(UiLine::CommandOutput(
                t(Msg::WorktreeDoneBack { path: &path }).into_owned(),
            ));
            if let Some(branch) = branch {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::WorktreeDoneMergeHint { branch: &branch }).into_owned(),
                ));
            }
        }
        SessionTransitionEffect::CleanupCurrentWorktree {
            manager_dir,
            target_dir,
            branch,
            force,
        } => match WorktreeManager::from_dir(manager_dir)
            .and_then(|manager| manager.remove(&branch, force))
        {
            Ok(()) => {
                renderer.render(UiLine::CommandOutput(
                    t(Msg::WorktreeCleaned { branch: &branch }).into_owned(),
                ));
                let path = target_dir.display().to_string();
                renderer.render(UiLine::CommandOutput(
                    t(Msg::WorktreeCleanedSwitched { path: &path }).into_owned(),
                ));
            }
            Err(error) => {
                let message = format!("{error:#}");
                if !force
                    && (message.contains("untracked")
                        || message.contains("modified")
                        || message.contains("changes"))
                {
                    renderer.render(UiLine::CommandOutput(
                        t(Msg::WorktreeCleanupUncommitted { branch: &branch }).into_owned(),
                    ));
                } else {
                    renderer.render(UiLine::Error(
                        t(Msg::WorktreeCleanupFailed { error: &message }).into_owned(),
                    ));
                }
            }
        },
    }
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

pub(crate) fn paths_same(a: &std::path::Path, b: &std::path::Path) -> bool {
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

/// `/status` login line: the signed-in identity (already formatted, e.g.
/// `昵称(用户名)`), or a not-signed-in prompt. Pure over the resolved string.
fn render_login_line(user: Option<&str>) -> String {
    match user {
        Some(u) => t(Msg::StatusLoginLoggedIn { user: u }).into_owned(),
        None => t(Msg::StatusLoginNotSignedIn).into_owned(),
    }
}

/// Format the signed-in identity as `display_name(username)` — the agreed
/// `昵称(用户名)` form. Falls back to just `username` when there is no distinct
/// display name: name absent, empty/whitespace, or identical to the username
/// (so we never render `Saulcy(Saulcy)`).
fn format_login_identity(name: Option<&str>, username: &str) -> String {
    match name
        .map(str::trim)
        .filter(|n| !n.is_empty() && *n != username)
    {
        Some(n) => format!("{n}({username})"),
        None => username.to_string(),
    }
}

/// The `/status` login line sourced from stored auth: `昵称(用户名)` (display
/// name + username), or just the username when there is no distinct display
/// name. Shared by both `/status` renderers so the interactive and remote
/// outputs can't drift.
fn render_login_line_from_stored_auth() -> String {
    match atomcode_auth::get_stored_auth() {
        Some(a) => {
            let identity = format_login_identity(a.user.name.as_deref(), &a.user.username);
            render_login_line(Some(&identity))
        }
        None => render_login_line(None),
    }
}

/// Render a CodingPlan auth failure. An EXPIRED login (`is_auth_expired` on the error
/// chain — dead local token, or a 401 from the server) → a clear localized "run
/// /login" prompt; otherwise `fallback()` (a genuine not-signed-in hint, or the raw
/// fetch-failure line). `from_stored_auth` returns `AuthExpired` for a dead token but a
/// PLAIN error when never logged in, so the two stay distinguishable.
fn render_cp_auth_error(e: &anyhow::Error, fallback: impl FnOnce() -> String) -> String {
    if atomcode_codingplan::is_auth_expired(e) {
        t(Msg::StatusCpAuthExpired).into_owned()
    } else {
        fallback()
    }
}

/// Fetch + format the CodingPlan section appended to `/status`. Runs a
/// blocking HTTP call (~100–500ms) against `/coding-plan/status` — same
/// endpoint as the `/codingplan` flow's step 4. Falls back to a one-line
/// hint when the user isn't signed in, has no active plan, or the API
/// call fails. Never panics and never returns an error: `/status` is a
/// quick-glance command, so any fetch problem degrades into a visible
/// note instead of aborting the whole command.
fn render_codingplan_status_for_status_cmd() -> String {
    tokio::task::block_in_place(|| {
        use atomcode_codingplan::client::Client;

        let client = match Client::from_stored_auth() {
            Ok(c) => c,
            // Expired login → clear re-login prompt; genuinely not signed in → the
            // not-signed-in hint. Without this split a dead token showed "not signed in"
            // while the Login line above said "signed in as X" — contradictory.
            Err(e) => return render_cp_auth_error(&e, || t(Msg::StatusCpNotSignedIn).into_owned()),
        };
        let status = match client.status_v2() {
            Ok(s) => s,
            Err(e) => {
                return render_cp_auth_error(&e, || {
                    t(Msg::StatusCpFetchFailed {
                        error: &format!("{:#}", e),
                    })
                    .into_owned()
                })
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
        })
        .into_owned();
        // Prefer the per-window `rate_limit_windows` schema when present, mirroring
        // `/login` (setup.rs). Iterate visible short windows (show_enable=1) normally.
        if !status.rate_limit_windows.is_empty() {
            use atomcode_codingplan::setup::format_duration_secs;
            for w in status
                .rate_limit_windows
                .iter()
                .filter(|w| w.show_enable == 1)
            {
                out.push_str(&t(Msg::StatusCpUsage {
                    usage: &w.usage_status_desc,
                    reset_at: &w.reset_at_display,
                    duration: &format_duration_secs(w.seconds_until_reset),
                }));
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
                duration: &atomcode_codingplan::setup::format_duration_secs(u.seconds_until_reset),
            }));
        }
        out
    })
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
        msg_count = t(Msg::CtxMessagesInWindow {
            n: snap.total_messages
        }),
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

/// Assemble the `/status` body in canonical display order: the login line FIRST
/// (so you see who you're signed in as at a glance), then the model/dir/config
/// block, the CodingPlan section, an optional Proxy line (interactive `/status`
/// only — the remote/phone view omits it), a blank separator, then the
/// instruction-files block. Pure over its already-rendered pieces so the order is
/// unit-testable and the interactive + remote renderers can't drift apart.
fn assemble_status(
    login: &str,
    body: &str,
    codingplan: &str,
    proxy: Option<&str>,
    instructions: &str,
) -> String {
    let mut txt = String::with_capacity(
        login.len() + body.len() + codingplan.len() + instructions.len() + 16,
    );
    txt.push_str(login);
    txt.push_str(body);
    txt.push_str(codingplan);
    if let Some(p) = proxy {
        txt.push_str(p);
    }
    txt.push('\n');
    txt.push_str(instructions);
    txt
}

/// `/status` 的报告文本。TUI arm 与手机远程执行（run_remote_command）共用。
/// `proxy` = 交互式 `/status` 传入的 Proxy 行；远程视图传 `None` 省略。
pub(super) fn build_status_text(ctx: &LoopCtx, proxy: Option<&str>) -> String {
    let body = t(Msg::StatusBody {
        model: &ctx.model_name,
        dir: &ctx.working_dir.display().to_string(),
        config: &ctx.config_store.path().display().to_string(),
    })
    .into_owned();
    assemble_status(
        &render_login_line_from_stored_auth(),
        &body,
        &render_codingplan_status_for_status_cmd(),
        proxy,
        &render_context_file_status_block(&ctx.working_dir),
    )
}

/// `/whoami` 的账号信息文本。TUI arm 与手机远程执行共用。
pub(super) fn build_whoami_text() -> String {
    if let Some(auth) = atomcode_auth::get_stored_auth() {
        let email = auth.user.email.as_deref().unwrap_or("—");
        let name = auth.user.name.as_deref().unwrap_or(&auth.user.username);
        format!(
            "  {} ({})\n  {}\n  auth: {}\n",
            name,
            auth.user.username,
            email,
            atomcode_auth::auth_file_path().display(),
        )
    } else {
        t(Msg::CmdWhoamiNotSignedIn).into_owned()
    }
}

/// Resolve a user-typed `/view <path>` argument to an absolute-ish path.
/// Expands a leading `~`/`~/` to the home dir and accepts absolute paths as-is;
/// anything else is joined onto the working dir. This is what lets `/view` open
/// files OUTSIDE the project (`/view ~/x`, `/view /abs/x`).
fn resolve_view_path(input: &str, working_dir: &std::path::Path) -> std::path::PathBuf {
    use std::path::PathBuf;
    let expanded: PathBuf = if input == "~" {
        crate::platform::home_dir().unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        match crate::platform::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(input),
        }
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        working_dir.join(expanded)
    }
}

/// Compact `/diff` summary used by the phone/remote command surface. The
/// interactive TUI renders file-scoped unified hunks instead.
pub(super) fn build_diff_stat_text(ctx: &LoopCtx) -> Result<String, String> {
    let snapshot = crate::git_diff::capture_diff_snapshot(&ctx.working_dir)
        .map_err(|error| t(Msg::DiffFailed { error: &error }).into_owned())?;
    if snapshot.files.is_empty() {
        return Ok(t(Msg::CmdNoChanges).into_owned());
    }
    Ok(crate::git_diff::format_compact_snapshot(&snapshot))
}

/// Fetch CodingPlan usage from the gateway (BLOCKING network call). `None` when the
/// user isn't logged into a CodingPlan account — the caller then shows
/// `UsageCodingPlanOnly`. Shared by the interactive modal (`open_usage`) and the
/// mid-turn footer report; both now render all three tabs.
///
/// Two round-trips: `status_v2` (plan + window) and the heavier `usage()` that powers
/// the Overview/Models tabs.
fn fetch_usage_data() -> Option<UsageData> {
    tokio::task::block_in_place(|| {
        let client = atomcode_codingplan::client::Client::from_stored_auth().ok()?;
        let status = client.status_v2().ok();
        let window = status.as_ref().and_then(|s| {
            s.rate_limit_windows
                .iter()
                .filter(|w| w.show_enable == 1)
                .filter(|w| w.window_hours > 0)
                .min_by_key(|w| w.window_hours)
                .cloned()
        });
        let plan = status.and_then(|s| s.codingplan_free);
        let (usage, error) = match client.usage() {
            Ok(u) => (Some(u), None),
            Err(e) => (None, Some(format!("{e}"))),
        };
        let overview = usage
            .as_ref()
            .map(atomcode_codingplan::usage::compute_overview);
        Some(UsageData {
            window,
            plan,
            usage,
            overview,
            error,
        })
    })
}

/// `/usage` — open the CodingPlan usage modal (idle). Renders a notice when the user
/// isn't on a CodingPlan account, otherwise pushes the modal into `active_modal`.
fn open_usage(renderer: &mut dyn Renderer, active_modal: &mut Option<Box<dyn Modal>>) {
    match fetch_usage_data() {
        Some(data) => *active_modal = Some(Box::new(UsageModal::new(data))),
        None => {
            renderer.render(UiLine::CommandOutput(
                t(Msg::UsageCodingPlanOnly).into_owned(),
            ));
            renderer.flush();
        }
    }
}

/// `/cost` 的用量报告文本：本会话累计 token × 模型价目表。与 `/usage`（只查
/// CodingPlan 网关）不同，这是本地统计，任何模型（含自接入）都能出数。TUI arm
/// 与手机远程执行共用。
pub(crate) fn build_cost_report_text(
    mut report: atomcode_capabilities::session::SessionCostReport,
    config: &atomcode_config::config::Config,
    current_provider: &str,
    current_model: &str,
    current_pricing: Option<atomcode_capabilities::session::ModelPricing>,
) -> String {
    if !report
        .models
        .iter()
        .any(|item| item.provider_id == current_provider && item.model_id == current_model)
    {
        report
            .models
            .push(atomcode_capabilities::session::ModelCostSummary {
                provider_id: current_provider.to_string(),
                model_id: current_model.to_string(),
                tokens: Default::default(),
                estimated_cost_usd: current_pricing.map(|_| 0.0),
                explicitly_free: current_pricing.is_some_and(|pricing| pricing.is_free()),
            });
    }

    // Resolve a selection id to its account for a friendly `account · model`
    // header (folded CodingPlan models share one `AtomGit` account); fall back to
    // the raw id when it isn't in the catalog (e.g. a since-removed provider).
    let catalog = config.logical_models();
    let account_of = |pid: &str| -> String {
        catalog
            .get(pid)
            .map(|m| m.account.clone())
            .unwrap_or_else(|| pid.to_string())
    };
    let mut sections = Vec::new();
    for item in report.models {
        let prompt = item.tokens.input.saturating_add(item.tokens.cached_input) as usize;
        let completion = item.tokens.output as usize;
        let cached = item.tokens.cached_input as usize;
        let cache_rate = if prompt == 0 {
            0
        } else {
            cached.saturating_mul(100) / prompt
        };
        let total = prompt.saturating_add(completion);
        let body = if item.explicitly_free {
            let cost = t(Msg::CostFree);
            t(Msg::CostReport {
                prompt,
                completion,
                cached,
                cache_rate,
                total,
                cost: &cost,
            })
        } else if let Some(estimated) = item.estimated_cost_usd {
            let cost = crate::pricing::format_cost(estimated);
            t(Msg::CostReport {
                prompt,
                completion,
                cached,
                cache_rate,
                total,
                cost: &cost,
            })
        } else {
            t(Msg::CostTokenReport {
                prompt,
                completion,
                cached,
                cache_rate,
                total,
            })
        };
        sections.push(format!(
            "{} · {}\n{}",
            account_of(&item.provider_id),
            item.model_id,
            body
        ));
    }
    if report.unattributed_tokens > 0 {
        sections.push(
            t(Msg::CostUnattributed {
                tokens: report.unattributed_tokens,
            })
            .into_owned(),
        );
    }
    sections.join("\n\n")
}

fn build_session_cost_text(ctx: &LoopCtx, state: &UiState) -> String {
    // The CURRENT-turn row must pair the ACTIVE selection with the active model.
    // Use the resolved selection id (matches `ctx.model_name`), not the possibly
    // stale legacy `default_provider` — otherwise the row mislabels e.g.
    // "agnes-ai · GLM-5.2".
    let provider = ctx.config.effective_model_selection().unwrap_or_default();
    let pricing = ctx
        .config
        .provider_config_for_selection(&provider)
        .and_then(|config| atomcode_coding::resolve_provider_pricing(&provider, &config));
    let manager = session_manager_for_cost(
        ctx.current_session_project_bucket.as_deref(),
        &ctx.current_session.working_dir,
    );
    let mut report = match manager.read_meta(&ctx.current_session.id) {
        Ok(meta) => atomcode_capabilities::session::aggregate_session_cost(&meta),
        Err(_) => {
            // Never relabel older in-memory totals as the current model when
            // native metadata is unavailable. Only the active turn below has
            // a trustworthy current-generation identity.
            let session_total = state.prompt_tokens.saturating_add(state.completion_tokens) as u64;
            let live_total = state
                .turn_prompt_tokens
                .saturating_add(state.turn_completion_tokens) as u64;
            let unattributed_tokens = session_total.saturating_sub(live_total);
            atomcode_capabilities::session::SessionCostReport {
                models: Vec::new(),
                unattributed_tokens,
                total_tokens: unattributed_tokens,
                estimated_cost_usd: None,
            }
        }
    };
    let live_tokens = atomcode_capabilities::session::TokenBreakdown {
        input: state
            .turn_prompt_tokens
            .saturating_sub(state.turn_cached_tokens) as u64,
        output: state.turn_completion_tokens as u64,
        cached_input: state.turn_cached_tokens as u64,
    };
    if live_tokens.total() > 0 {
        if let Some(model) = report
            .models
            .iter_mut()
            .find(|item| item.provider_id == provider && item.model_id == ctx.model_name)
        {
            model.tokens.input = model.tokens.input.saturating_add(live_tokens.input);
            model.tokens.output = model.tokens.output.saturating_add(live_tokens.output);
            model.tokens.cached_input = model
                .tokens
                .cached_input
                .saturating_add(live_tokens.cached_input);
            match (model.estimated_cost_usd.as_mut(), pricing) {
                (Some(cost), Some(price)) => *cost += price.estimate(live_tokens),
                _ => model.estimated_cost_usd = None,
            }
            model.explicitly_free &= pricing.is_some_and(|price| price.is_free());
        } else {
            report
                .models
                .push(atomcode_capabilities::session::ModelCostSummary {
                    provider_id: provider.to_string(),
                    model_id: ctx.model_name.clone(),
                    tokens: live_tokens,
                    estimated_cost_usd: pricing.map(|price| price.estimate(live_tokens)),
                    explicitly_free: pricing.is_some_and(|price| price.is_free()),
                });
        }
        report.total_tokens = report.total_tokens.saturating_add(live_tokens.total());
        report.estimated_cost_usd = None;
    }
    build_cost_report_text(report, &ctx.config, &provider, &ctx.model_name, pricing)
}

fn session_manager_for_cost(
    project_bucket: Option<&str>,
    working_dir: &std::path::Path,
) -> atomcode_capabilities::session::SessionManager {
    project_bucket
        .filter(|bucket| bucket.len() == 16 && bucket.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(|bucket| {
            atomcode_capabilities::session::SessionManager::with_root(
                atomcode_capabilities::session::SessionManager::sessions_root().join(bucket),
            )
        })
        .unwrap_or_else(|| atomcode_capabilities::session::SessionManager::for_project(working_dir))
}

#[cfg(test)]
mod cost_session_location_tests {
    use super::session_manager_for_cost;

    #[test]
    fn authoritative_catalog_bucket_wins_over_working_dir_hash() {
        let bucket = "0123456789abcdef";
        let manager = session_manager_for_cost(Some(bucket), std::path::Path::new("/different"));
        assert_eq!(
            manager.root(),
            atomcode_capabilities::session::SessionManager::sessions_root().join(bucket)
        );
    }
}

/// `/schedule` list text (pure function, easy to test).
/// Empty → usage hint; otherwise one line per task: id | title | next | last | enabled.
pub(crate) fn build_schedule_list_text(
    tasks: &[atomcode_config::schedule::ScheduleTask],
    now: i64,
) -> String {
    if tasks.is_empty() {
        return "  No scheduled tasks. Use `atomcode schedule add` to create one.\n".to_string();
    }
    let mut out = String::from("  Scheduled tasks:\n\n");
    for t in tasks {
        let next = atomcode_config::schedule::next_run(&t.schedule, now)
            .map(|ts| format!("{ts}"))
            .unwrap_or_else(|| "-".to_string());
        let en = if t.enabled { "on" } else { "off" };
        out.push_str(&format!(
            "  {} | {} | next:{} | last:{} | {}\n",
            t.id,
            t.title,
            next,
            t.last_status.as_deref().unwrap_or("-"),
            en
        ));
    }
    out
}

#[cfg(test)]
mod schedule_list_text_tests {
    use super::build_schedule_list_text;
    use atomcode_config::schedule::{Schedule, ScheduleTask};

    fn make_task(id: &str, title: &str, enabled: bool, last_status: Option<&str>) -> ScheduleTask {
        ScheduleTask {
            id: id.to_string(),
            title: title.to_string(),
            prompt: "do something".to_string(),
            cwd: "/tmp".to_string(),
            schedule: Schedule::Daily { time: "09:00".to_string() },
            permission_mode: "plan".to_string(),
            notify: "important".to_string(),
            enabled,
            created_at: 0,
            last_run_at: None,
            last_status: last_status.map(|s| s.to_string()),
        }
    }

    #[test]
    fn empty_list_shows_hint() {
        let out = build_schedule_list_text(&[], 0);
        assert!(
            out.contains("No scheduled tasks"),
            "empty list should mention No scheduled tasks, got: {out}"
        );
        assert!(
            out.contains("atomcode schedule add"),
            "empty list should mention add command, got: {out}"
        );
    }

    #[test]
    fn two_tasks_shown_in_order_with_id_title_enabled() {
        let tasks = vec![
            make_task("task-1", "Daily brief", true, Some("ok")),
            make_task("task-2", "Weekly report", false, None),
        ];
        let now = 1785657600_i64; // 2026-07-31 08:00 UTC
        let out = build_schedule_list_text(&tasks, now);
        assert!(out.contains("task-1"), "should contain first task id");
        assert!(out.contains("Daily brief"), "should contain first task title");
        assert!(out.contains("on"), "enabled task should show 'on'");
        assert!(out.contains("task-2"), "should contain second task id");
        assert!(out.contains("Weekly report"), "should contain second task title");
        assert!(out.contains("off"), "disabled task should show 'off'");
        // Order: task-1 line comes before task-2 line
        let pos1 = out.find("task-1").unwrap();
        let pos2 = out.find("task-2").unwrap();
        assert!(pos1 < pos2, "task-1 should appear before task-2");
    }
}

/// 手机端可远程触发的**只读信息类**命令白名单。返回 None = 不允许远程执行
/// （交互式/桌面专属命令一律拒绝，由调用方回话术）。
pub(super) fn run_remote_command(ctx: &LoopCtx, state: &UiState, cmd: &str) -> Option<String> {
    match cmd
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase()
        .as_str()
    {
        "status" => Some(build_status_text(ctx, None)),
        "cost" => Some(build_session_cost_text(ctx, state)),
        "whoami" => Some(build_whoami_text()),
        "diff" => Some(build_diff_stat_text(ctx).unwrap_or_else(|e| e)),
        _ => None,
    }
}

/// Ask CodingRuntime to atomically replace the current session. The runtime
/// terminal owns the UI/session projection commit; this function deliberately
/// does not clear the current screen or bind a locally invented session.
pub(crate) fn reset_to_new_session(
    ctx: &mut LoopCtx,
    state: &mut UiState,
    renderer: &mut dyn Renderer,
) {
    if provider_transition_pending(ctx) {
        renderer.render(UiLine::Error(t(Msg::CmdProviderReloading).into_owned()));
        renderer.flush();
        return;
    }
    if ctx.pending_session_transition.is_some()
        || ctx.pending_session_resume.is_some()
        || ctx.pending_session_resume_preparation.is_some()
        || ctx.pending_capability_reload
    {
        renderer.render(UiLine::Warning(
            t(Msg::CmdSessionTransitionPending).into_owned(),
        ));
        renderer.flush();
        return;
    }
    // /clear and /session must also halt any active /loop (both self-paced
    // runtime and fixed-interval TUI controller).
    stop_active_loop(state, ctx);
    match ctx
        .runtime
        .fresh_session(ctx.foreground_runtime_id, ctx.runtime_event_tx.clone())
    {
        Ok(()) => {
            ctx.pending_session_transition = Some(crate::event_loop::PendingSessionTransition {
                operation: atomcode_coding::ReconfigureKind::FreshSession,
                requested_working_dir: ctx.working_dir.clone(),
                committed: None,
                effect: crate::event_loop::SessionTransitionEffect::None,
            });
            // Success is fast (the reconfigure connects MCP in the background and
            // never blocks): the transition terminal wipes the screen / re-renders
            // shortly, so the "reconfiguring…" status is just noise here. It's still
            // shown by the guards above / on submit while a transition is pending.
        }
        Err(error) => renderer.render(UiLine::Error(
            t(Msg::CmdSessionTransitionFailed {
                error: &error.to_string(),
            })
            .into_owned(),
        )),
    }
    renderer.flush();
}

/// Start an atomic fresh-session transition into a new working directory.
/// The correlated runtime terminal performs the projection commit; callers
/// must not update cwd, recent directories, or live state optimistically.
pub(crate) fn apply_cd(ctx: &mut LoopCtx, path: PathBuf) -> Result<PathBuf, String> {
    apply_cd_with_effect(ctx, path, crate::event_loop::SessionTransitionEffect::None)
}

fn apply_cd_with_effect(
    ctx: &mut LoopCtx,
    path: PathBuf,
    effect: crate::event_loop::SessionTransitionEffect,
) -> Result<PathBuf, String> {
    // Normalize the funnel: `resolve_cd` strips the Windows `\\?\` verbatim prefix,
    // but the dir-picker's recent-list branch and the webui `ProjectSwitched` event
    // reach here WITHOUT going through it, carrying a canonicalized `\\?\C:\…` path
    // (persisted recent_dirs.txt entries from before the fix, or a re-canonicalized
    // runtime value). Strip here so `working_dir`, `recent_dirs`, the `ChangeDirectory`
    // command, and the webui sync all store the plain form regardless of caller.
    let path = atomcode_capabilities::pathnorm::strip_verbatim_path(&path);
    if provider_transition_pending(ctx) {
        return Err(t(Msg::CmdProviderReloading).into_owned());
    }
    if ctx.pending_session_transition.is_some()
        || ctx.pending_session_resume.is_some()
        || ctx.pending_session_resume_preparation.is_some()
        || ctx.pending_capability_reload
    {
        return Err(t(Msg::CmdSessionTransitionPending).into_owned());
    }
    ctx.runtime
        .change_directory(
            path.clone(),
            ctx.foreground_runtime_id,
            ctx.runtime_event_tx.clone(),
        )
        .map_err(|error| {
            t(Msg::CmdSessionTransitionFailed {
                error: &error.to_string(),
            })
            .into_owned()
        })?;
    ctx.pending_session_transition = Some(crate::event_loop::PendingSessionTransition {
        operation: atomcode_coding::ReconfigureKind::ChangeDirectory,
        requested_working_dir: path.clone(),
        committed: None,
        effect,
    });
    Ok(path)
}

/// Move `new` to the front of `dirs`, dedup, and cap at `MAX_RECENT_DIRS`.
/// Does NOT persist — call `save_recent_dirs` after, or use `apply_cd`
/// which does both.
pub(crate) fn push_recent_dir(dirs: &mut Vec<PathBuf>, new: PathBuf) {
    // De-dup case-insensitively on case-insensitive filesystems so `C:\Users`
    // and `C:\users` (same physical dir) don't both linger in the picker.
    let key = atomcode_capabilities::pathnorm::path_case_key(&new);
    dirs.retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != key);
    dirs.insert(0, new);
    dirs.truncate(MAX_RECENT_DIRS);
}

/// Parse `recent_dirs.txt` contents into a `\\?\`-stripped, de-duplicated path
/// list, preserving first-occurrence order. Pure (no filesystem) so it is
/// unit-testable; the `is_dir` liveness filter + `MAX_RECENT_DIRS` cap stay in
/// `load_recent_dirs` because they touch the FS.
///
/// De-dup matters because a legacy file can hold BOTH the `\\?\C:\…` verbatim
/// form and the plain `C:\…` form of the same dir (cd'd on an old vs a fixed
/// binary), OR the same dir in two cases (`C:\Users` vs `C:\users`). Stripping
/// collapses the verbatim form and the case-insensitive key collapses the case
/// variants, so the picker shows one `~/atomcode` row, not two. `push_recent_dir`
/// only de-dups on WRITE — this handles the READ side for pre-existing files.
fn parse_recent_dirs(contents: &str) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(PathBuf::from)
        .map(|p| atomcode_capabilities::pathnorm::strip_verbatim_path(&p))
        .filter(|p| seen.insert(atomcode_capabilities::pathnorm::path_case_key(p)))
        .collect()
}

/// Read `~/.atomcode/recent_dirs.txt`. Silently drops missing directories
/// so stale entries from a deleted project don't linger in the picker.
pub(crate) fn load_recent_dirs() -> Vec<PathBuf> {
    let path = atomcode_config::config::Config::config_dir().join("recent_dirs.txt");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| {
            parse_recent_dirs(&s)
                .into_iter()
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
    let path = atomcode_config::config::Config::config_dir().join("recent_dirs.txt");
    let content = dirs
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(&path, content);
}

/// Build the complete `/cd` project list. Catalog projects retain newest-session
/// order. A current directory with no history is prepended so it remains visible;
/// MRU-only directories follow the catalog. Invalid/deleted paths are omitted and
/// Windows case/verbatim aliases collapse to one row.
pub(crate) fn load_cd_picker_dirs(current: &Path, recent: &[PathBuf]) -> Vec<PathBuf> {
    let scan = atomcode_capabilities::session::SessionManager::scan_catalog(
        &atomcode_capabilities::session::SessionManager::sessions_root(),
    );
    merge_cd_picker_dirs(
        current,
        recent,
        scan.entries.into_iter().map(|entry| entry.working_dir),
        |path| path.is_dir(),
    )
}

fn merge_cd_picker_dirs<I, F>(
    current: &Path,
    recent: &[PathBuf],
    catalog_dirs: I,
    mut is_dir: F,
) -> Vec<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
    F: FnMut(&Path) -> bool,
{
    let current = atomcode_capabilities::pathnorm::strip_verbatim_path(current);
    let mut catalog_seen = std::collections::HashSet::new();
    let catalog_dirs = catalog_dirs
        .into_iter()
        .map(|path| atomcode_capabilities::pathnorm::strip_verbatim_path(&path))
        .filter(|path| {
            catalog_seen.insert(atomcode_capabilities::pathnorm::path_case_key(path))
        })
        .filter(|path| is_dir(path))
        .collect::<Vec<_>>();
    let current_key = atomcode_capabilities::pathnorm::path_case_key(&current);
    let current_in_catalog = catalog_dirs
        .iter()
        .any(|path| atomcode_capabilities::pathnorm::path_case_key(path) == current_key);
    let mut seen = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    if !current_in_catalog && is_dir(&current) && seen.insert(current_key) {
        dirs.push(current);
    }
    for path in catalog_dirs {
        if seen.insert(atomcode_capabilities::pathnorm::path_case_key(&path)) {
            dirs.push(path);
        }
    }
    for path in recent {
        let path = atomcode_capabilities::pathnorm::strip_verbatim_path(path);
        let key = atomcode_capabilities::pathnorm::path_case_key(&path);
        if seen.insert(key) && is_dir(&path) {
            dirs.push(path);
        }
    }
    dirs
}

pub(crate) fn resolve_cd(
    arg: &str,
    cwd: &std::path::Path,
    prev: Option<&std::path::Path>,
) -> std::result::Result<PathBuf, String> {
    let home = crate::platform::home_dir();
    let target = expand_cd_target(arg, home.as_deref(), cwd, prev)?;
    let canon = target
        .canonicalize()
        .map_err(|e| format!("{}: {}", target.display(), e))?;
    // On Windows `canonicalize` returns a `\\?\` verbatim / extended-length path.
    // Strip it here at the SOURCE so every downstream sink carries the plain
    // `C:\…` form: the "已切换到 …" confirmation (uses this value directly), the
    // stored `working_dir`, the change-directory request sent to the runtime, the
    // webui footer sync (`live_set_working_dir`), and `recent_dirs.txt`. Only the
    // status-row `collapse_home` stripped before, so those other sites leaked the
    // raw `\\?\C:\Users\hao\atomcode`. Mirrors the daemon's `change_dir`, which
    // already strips before setting its working dir. No-op off Windows / on
    // non-verbatim paths; `hash_path` strips internally so the session bucket is
    // unchanged.
    let canon = atomcode_capabilities::pathnorm::strip_verbatim_path(&canon);
    if !canon.is_dir() {
        return Err(t(Msg::DirNotADirectory {
            path: &canon.display().to_string(),
        })
        .into_owned());
    }
    Ok(canon)
}

/// Expand a `/cd` argument to a target path WITHOUT touching the filesystem (no
/// canonicalize / existence check — the caller does that). Handles `~`, `~/sub`,
/// `~\sub` (Windows backslash), `-` (previous dir), absolute, and relative-to-cwd.
/// Pure (filesystem-free) so the path logic is unit-testable; `resolve_cd` wraps
/// it with the canonicalize + is_dir validation.
pub(crate) fn expand_cd_target(
    arg: &str,
    home: Option<&std::path::Path>,
    cwd: &std::path::Path,
    prev: Option<&std::path::Path>,
) -> std::result::Result<PathBuf, String> {
    if arg.is_empty() {
        return home
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| "home directory not known".to_string());
    }
    if arg == "-" {
        return prev
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| "No previous directory".to_string());
    }
    if let Some(rest) = arg.strip_prefix('~') {
        let home = home.ok_or_else(|| "home directory not known".to_string())?;
        // Strip the leading separator(s) after `~` — BOTH `/` and `\` so a Windows
        // user can type `~\Desktop` like `~/Desktop`, and ALL of them so a doubled
        // separator (`~//x`, easy typo) doesn't leave an absolute remnant that
        // `home.join` would treat as a root and escape the home dir.
        let rest = rest.trim_start_matches(['/', '\\']);
        return Ok(if rest.is_empty() {
            home.to_path_buf()
        } else {
            home.join(rest)
        });
    }
    let p = PathBuf::from(arg);
    Ok(if p.is_absolute() { p } else { cwd.join(p) })
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

/// Extract the verbatim bodies of fenced (```` ``` ```` / `~~~`) code blocks
/// from markdown, in document order. Used by `/copy` to recover the ORIGINAL
/// unwrapped command text — never the rendered body cells, which are already
/// hard-wrapped + PAD-indented and would corrupt a pasted command.
///
/// A fence opens on a line whose trimmed form starts with three or more of the
/// fence char (an info string like ```` ```bash ```` is fine) and closes on a
/// line that is ONLY fence chars of the same kind. Inner lines are kept
/// verbatim (their own indentation preserved). An unterminated fence (a reply
/// truncated mid-stream) still yields what was captured.
fn extract_code_blocks(md: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut inner: Vec<&str> = Vec::new();
    let mut in_block = false;
    let mut fence_char = '`';
    let mut fence_len = 3;
    for line in md.lines() {
        let t = line.trim();
        if !in_block {
            if let Some((c, len)) = fence_start(t) {
                in_block = true;
                fence_char = c;
                fence_len = len;
                inner.clear();
            }
        } else if is_closing_fence(t, fence_char, fence_len) {
            blocks.push(inner.join("\n"));
            in_block = false;
        } else {
            inner.push(line);
        }
    }
    if in_block {
        blocks.push(inner.join("\n"));
    }
    blocks
}

/// Outcome of resolving a `/copy [arg]` request against a reply's markdown.
#[derive(Debug)]
enum CopyResolve {
    /// The text to place on the clipboard. The bool is `true` when this came
    /// from `/copy msg` (the full reply) so the caller can use a confirmation
    /// message that says "reply" rather than "code block".
    Text(String, bool),
    /// The reply has no fenced code block (or there's no reply yet).
    NoBlocks,
    /// `/copy msg` was used but the reply is empty/whitespace-only.
    /// Distinct from `NoBlocks` so the caller can surface a "reply is empty"
    /// hint rather than the misleading "no code block" wording.
    EmptyMsg,
    /// `/copy N` referenced an out-of-range index; carries the block count.
    BadIndex(usize),
}

/// Outcome of `/save [filename]` — either the conversation was written to a
/// file (carrying the resolved path for display) or it failed for one of three
/// reasons: nothing to export, an I/O error, or an invalid/unsafe path.
#[derive(Debug)]
enum SaveOutcome {
    /// File written successfully; carries the resolved absolute path.
    Ok(std::path::PathBuf),
    /// The session has no exportable conversation turns yet.
    EmptyHistory,
    /// The underlying filesystem write failed; carries the error message.
    IoError(String),
    /// The requested path is invalid or its parent directory does not exist.
    InvalidPath(String),
    /// The target already exists and is NOT a markdown file — refuse to clobber
    /// it (a `/save mydata.py` typo would otherwise overwrite source/config with
    /// the transcript). Carries the target path for the message.
    RefuseOverwrite(String),
}

/// Expand a leading `~` / `~/` in `arg` to `home`, mirroring shell behaviour so
/// `/save ~/notes.md` lands in the home dir instead of a literal `./~/` folder
/// (consistent with read_file / glob, which already expand `~`). Pure over
/// `home` so it is unit-testable without touching the environment. A bare `~`
/// → home; `~/x` → home/x; `~user` and everything else pass through unchanged
/// (we don't resolve other users' homes). No home known → `arg` as-is.
fn expand_tilde_path(arg: &str, home: Option<&std::path::Path>) -> std::path::PathBuf {
    let Some(home) = home else {
        return std::path::PathBuf::from(arg);
    };
    if arg == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = arg.strip_prefix("~/") {
        return home.join(rest);
    }
    std::path::PathBuf::from(arg)
}

/// Whether `path`'s extension marks it as a markdown file (case-insensitive
/// `md` / `markdown`). Used to gate the "refuse to overwrite a non-markdown
/// file" guard.
fn is_markdown_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

/// Build the default export filename: `atomcode-session-YYYYMMDD-HHMMSS.md`.
/// Extracted from [`resolve_save_in`] so unit tests can check the naming scheme
/// without touching the filesystem (where parallel chdir would race).
fn default_save_filename() -> String {
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    format!("atomcode-session-{stamp}.md")
}

/// Render the session's exportable turns as a markdown transcript. Pure /
/// side-effect-free so it can be unit-tested independently of file I/O.
fn render_save_markdown(messages: &[atomcode_kernel::message::Message]) -> Option<String> {
    use atomcode_kernel::message::Role;
    let turns: Vec<(&Role, &str)> = messages
        .iter()
        .filter(|m| !m.synthetic && matches!(m.role, Role::User | Role::Assistant))
        .map(|m| (&m.role, m.text.as_str()))
        .filter(|(_, t)| !t.trim().is_empty())
        .collect();
    if turns.is_empty() {
        return None;
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut out = String::new();
    out.push_str(&format!("# AtomCode Session - {now}\n\n"));
    for (role, text) in &turns {
        let label = match role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            // bot review Low: filter 上游已限定为 User|Assistant,该臂不可达。
            // 用 unreachable! 替代静默 continue,一旦未来 filter 放宽会立即 panic 暴露,而非悄悄丢消息。
            _ => unreachable!("render_save_markdown: role filtered to User|Assistant upstream"),
        };
        out.push_str(&format!("## {label}\n{text}\n\n"));
    }
    Some(out)
}

/// Map `/save [filename]` to a written file. `""` → a timestamped default
/// (`atomcode-session-YYYYMMDD-HHMMSS.md`) in the active project directory;
/// a bare name or relative path resolves against `working_dir`;
/// an absolute path is used as-is. Existing files are overwritten.
fn resolve_save_in(
    messages: &[atomcode_kernel::message::Message],
    arg: &str,
    working_dir: &std::path::Path,
) -> SaveOutcome {
    let Some(content) = render_save_markdown(messages) else {
        return SaveOutcome::EmptyHistory;
    };

    let arg = arg.trim();
    let path = if arg.is_empty() {
        std::path::PathBuf::from(default_save_filename())
    } else {
        // Expand `~` (sudo-aware home via crate::platform) so `/save ~/x.md`
        // works like it does in the shell / other file-taking commands.
        expand_tilde_path(arg, crate::platform::home_dir().as_deref())
    };
    let path = if path.is_absolute() {
        path
    } else {
        working_dir.join(path)
    };

    // Reject paths whose parent directory doesn't exist — we don't auto-mkdir,
    // so a typo can't silently scatter directories. Relative paths are already
    // rooted at `working_dir`; absolute paths are checked as provided.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return SaveOutcome::InvalidPath(parent.to_string_lossy().into_owned());
        }
    }

    // Refuse to overwrite an existing NON-markdown file: `/save` is a markdown
    // export, so a target like `config.py` / `.bashrc` / a bare `notes` that
    // already exists is almost certainly a typo, and clobbering it loses data.
    // Overwriting an existing `.md` (re-export) is fine; a NEW file of any name
    // is fine (no clobber). The default filename is always a fresh timestamped
    // `.md`, so it never trips this.
    if path.is_file() && !is_markdown_path(&path) {
        return SaveOutcome::RefuseOverwrite(path.to_string_lossy().into_owned());
    }

    match std::fs::write(&path, content) {
        Ok(()) => {
            // Canonicalize so SaveOutcome::Ok carries an absolute path as
            // documented (matches the "resolved absolute path" doc comment).
            // On the rare canonicalize failure (e.g. the file was removed
            // between write and canonicalize on some platforms), fall back
            // to the as-written path so the success isn't turned into an
            // error by a post-success race.
            let resolved = path.canonicalize().unwrap_or(path);
            SaveOutcome::Ok(resolved)
        }
        Err(e) => SaveOutcome::IoError(e.to_string()),
    }
}

/// Map `/copy [arg]` to the text to copy. `""` → last block (the common
/// "copy the command just shown" case); `all` → every block joined by a blank
/// line; `N` (1-based) → the Nth block; `msg` → the full reply markdown
/// (prose + code, useful for pasting the whole answer elsewhere).
fn resolve_copy(md: &str, arg: &str) -> CopyResolve {
    let arg = arg.trim();
    // `/copy msg` → full reply markdown (skip code-block extraction entirely).
    if arg.eq_ignore_ascii_case("msg") {
        let trimmed = md.trim();
        if trimmed.is_empty() {
            return CopyResolve::EmptyMsg;
        }
        return CopyResolve::Text(trimmed.to_string(), true);
    }
    let blocks = extract_code_blocks(md);
    if blocks.is_empty() {
        return CopyResolve::NoBlocks;
    }
    if arg.is_empty() {
        return CopyResolve::Text(blocks.last().cloned().unwrap_or_default(), false);
    }
    if arg.eq_ignore_ascii_case("all") {
        return CopyResolve::Text(blocks.join("\n\n"), false);
    }
    match arg.parse::<usize>() {
        Ok(n) if (1..=blocks.len()).contains(&n) => CopyResolve::Text(blocks[n - 1].clone(), false),
        _ => CopyResolve::BadIndex(blocks.len()),
    }
}

/// Write `text` to the system clipboard. Tries arboard (system clipboard
/// API) first; falls back to OSC 52 emitted to `stdout` for headless / SSH
/// sessions where no windowing system is available.
///
/// OSC 52 format: `\x1b]52;c;<base64>\x1b\\`
///
/// This is the public entry-point used by both the `/copy` command and the
/// retained renderer's auto-copy path (issue #699).
pub(crate) fn copy_text_to_clipboard_osc52(text: &str) -> bool {
    // Tier 1: system clipboard via arboard (desktop)
    if try_arboard_clipboard(text) {
        return true;
    }
    // Tier 2: OSC 52 escape sequence. Only emit when stdout is a real
    // terminal — piping OSC bytes into a file or another process is
    // meaningless (issue #699 P4).
    use std::io::IsTerminal as _;
    if !std::io::stdout().is_terminal() {
        return false;
    }
    write_osc52_clipboard_to(&mut std::io::stdout(), text)
}

/// Variant of [`copy_text_to_clipboard_osc52`] that emits the OSC 52
/// fallback through `writer` instead of raw stdout.  Retained-mode
/// renderers should use this with their own `BufWriter<Stdout>` so the
/// escape sequence stays ordered with buffered body/content writes.
pub(crate) fn copy_text_to_clipboard_osc52_via(
    writer: &mut impl std::io::Write,
    text: &str,
) -> bool {
    if try_arboard_clipboard(text) {
        return true;
    }
    write_osc52_clipboard_to(writer, text)
}

fn try_arboard_clipboard(text: &str) -> bool {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text.to_string()))
        .is_ok()
}

/// Emit an OSC 52 escape sequence through `writer`.
fn write_osc52_clipboard_to(writer: &mut impl std::io::Write, text: &str) -> bool {
    let seq = encode_osc52("c", text);
    writer.write_all(seq.as_bytes()).is_ok() && writer.flush().is_ok()
}

/// Build an OSC 52 escape sequence: `ESC ]52;<buffer>;<base64> ST`.
/// `buffer` is typically `"c"` (clipboard) or `"p"` (primary selection).
///
/// Note: some terminals cap OSC payloads at ~4096 bytes. For code blocks
/// longer than ~3 KB the OSC 52 path may be silently truncated; the arboard
/// desktop path (tier 1) has no such limit and will succeed first on any
/// machine with a windowing system.
pub(crate) fn encode_osc52(buffer: &str, text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;{};{}\x1b\\", buffer, b64)
}

/// Build the non-error rate-limit pause body line. Three branches:
/// - `auto_resuming` → kernel is auto-retrying (WaitAndRetry); generic countdown.
/// - Pause, `reset_at_display` NON-empty → a CONFIRMED CodingPlan quota exhaustion
///   (real reset time from the usage windows) → the CodingPlan "5h window" message.
/// - Pause, `reset_at_display` EMPTY → generic 429 (a user's external-model 429, or
///   a gateway 429 with no window data) → a neutral "limited (HTTP 429)" line, NOT
///   the CodingPlan message. The `RateLimitHook` gates itself to gateway 429s, so an
///   external-model 429 lands here via the kernel's generic default.
///
/// Kept as a pure function so it is unit-testable without a renderer.
pub(crate) fn format_rate_limited_line(
    reset_at_display: &str,
    reset_label: &str,
    secs_until_reset: Option<u64>,
    auto_resuming: bool,
    server_message: Option<&str>,
) -> String {
    if auto_resuming {
        // WaitAndRetry: kernel is sleeping then will retry automatically.
        let n = secs_until_reset.unwrap_or(0);
        return format!("⏳ 限流，{n}s 后自动继续…");
    }
    // Pause: kernel stopped, user must act. A CodingPlan verdict (decide_from_windows)
    // carries window data — a reset time AND/OR a window label. The kernel's generic
    // default (from_hint, used for non-CodingPlan / external-model 429s) carries
    // NEITHER. So "has any window signal" ⇒ a real CodingPlan quota; otherwise it's a
    // generic 429 and must NOT be dressed up as a CodingPlan quota exhaustion. Keying
    // on reset_at_display ALONE would wrongly go generic for an exhausted window whose
    // display string the server omitted (both fields are `#[serde(default)]`).
    let is_coding_plan = !reset_at_display.is_empty() || !reset_label.is_empty();
    if !is_coding_plan {
        let tail = match secs_until_reset {
            Some(s) => format!("（约 {} 后可重试）", fmt_dur(s)),
            None => String::new(),
        };
        // Surface the provider's OWN 429 reason when it carried one (e.g. an external
        // model's "余额不足…请充值") so the user sees the actionable cause, not a bare 429.
        let reason = match server_message {
            Some(m) if !m.trim().is_empty() => format!("：{}", m.trim()),
            _ => String::new(),
        };
        return format!("⏸ 限流（HTTP 429）{reason}{tail} · 已保留已完成内容 · 稍后重试或换模型");
    }
    // Confirmed CodingPlan window exhaustion.
    let tail = match secs_until_reset {
        Some(s) => format!("（还有 {}）", fmt_dur(s)),
        None => String::new(),
    };
    if reset_at_display.is_empty() {
        return format!(
            "⏸ 5小时窗口已用尽，稍后恢复{tail} · 已保留已完成内容 · 可换模型或稍后重试"
        );
    }
    format!(
        "⏸ 5小时窗口已用尽，约 {reset_at_display} 恢复{tail} · 已保留已完成内容 · 可换模型或稍后重试"
    )
}

/// Format a duration in seconds as a compact human string: "2h11m" / "45m" / "30s".
fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Recognised `/mcp` subcommands.
#[derive(Debug, PartialEq)]
pub(crate) enum McpSub {
    Reload,
    Tools,
    Login,
    Logout,
    Trust,
    Untrust,
}

/// Parse the argument string following `/mcp` into a known subcommand.
/// Returns `None` for unrecognised inputs (which fall through to status display).
pub(crate) fn parse_mcp_subcommand(sub: &str) -> Option<McpSub> {
    let s = sub.trim();
    if s.eq_ignore_ascii_case("reload") {
        Some(McpSub::Reload)
    } else if s.eq_ignore_ascii_case("trust") {
        Some(McpSub::Trust)
    } else if s.eq_ignore_ascii_case("untrust") {
        Some(McpSub::Untrust)
    } else if s.starts_with("tools") {
        Some(McpSub::Tools)
    } else if s.starts_with("login") {
        Some(McpSub::Login)
    } else if s.starts_with("logout") {
        Some(McpSub::Logout)
    } else {
        None
    }
}

#[cfg(test)]
mod status_login_tests {
    use super::*;

    #[test]
    fn login_line_shows_username_when_signed_in() {
        let line = render_login_line(Some("张三"));
        assert!(
            line.contains("张三"),
            "signed-in line must show the username: {line:?}"
        );
    }

    #[test]
    fn login_line_prompts_login_when_not_signed_in() {
        let line = render_login_line(None);
        assert!(
            line.contains("/login"),
            "not-signed-in line must point to /login: {line:?}"
        );
        assert!(!line.contains("张三"));
    }

    #[test]
    fn login_identity_is_name_paren_username() {
        // The agreed 昵称(用户名) form: display name with the username in parens.
        assert_eq!(
            format_login_identity(Some("TheoCui"), "Saulcy"),
            "TheoCui(Saulcy)"
        );
        assert_eq!(
            format_login_identity(Some("  Theo  "), "Saulcy"),
            "Theo(Saulcy)"
        );
    }

    #[test]
    fn login_identity_falls_back_to_bare_username() {
        // No distinct display name → just the username (never `Saulcy(Saulcy)`).
        assert_eq!(format_login_identity(None, "Saulcy"), "Saulcy");
        assert_eq!(format_login_identity(Some(""), "Saulcy"), "Saulcy");
        assert_eq!(format_login_identity(Some("   "), "Saulcy"), "Saulcy");
        assert_eq!(format_login_identity(Some("Saulcy"), "Saulcy"), "Saulcy");
    }

    #[test]
    fn status_order_is_login_first_then_body_codingplan_proxy() {
        // Reorder spec: login line at the very top; Proxy AFTER CodingPlan.
        let s = assemble_status(
            "LOGIN\n",
            "BODY\n",
            "CODINGPLAN\n",
            Some("PROXY\n"),
            "INSTRUCTIONS",
        );
        assert!(s.starts_with("LOGIN\n"), "login must be first: {s:?}");
        let (login, body, cp, proxy, instr) = (
            s.find("LOGIN").unwrap(),
            s.find("BODY").unwrap(),
            s.find("CODINGPLAN").unwrap(),
            s.find("PROXY").unwrap(),
            s.find("INSTRUCTIONS").unwrap(),
        );
        // login < body < codingplan < proxy < instructions
        assert!(
            login < body && body < cp,
            "body sits between login and codingplan: {s:?}"
        );
        assert!(cp < proxy, "Proxy must come AFTER CodingPlan: {s:?}");
        assert!(proxy < instr, "instructions come last: {s:?}");
    }

    #[test]
    fn status_omits_proxy_line_when_none() {
        // The remote/phone view passes None → no Proxy line at all.
        let s = assemble_status("LOGIN\n", "BODY\n", "CODINGPLAN\n", None, "INSTRUCTIONS");
        assert!(
            !s.contains("PROXY"),
            "proxy must be absent when None: {s:?}"
        );
        assert!(s.starts_with("LOGIN\n"), "login still first: {s:?}");
    }

    #[test]
    fn status_body_no_longer_shows_a_token_line() {
        // /status is a quick-glance state view; per-session token count is /cost's job.
        let en = atomcode_config::i18n::t_with(
            atomcode_config::i18n::Locale::En,
            Msg::StatusBody {
                model: "m",
                dir: "/d",
                config: "/c",
            },
        );
        let zh = atomcode_config::i18n::t_with(
            atomcode_config::i18n::Locale::ZhCn,
            Msg::StatusBody {
                model: "m",
                dir: "/d",
                config: "/c",
            },
        );
        assert!(
            !en.contains("Token"),
            "en StatusBody must not carry a Token line: {en}"
        );
        assert!(
            !zh.contains("Token"),
            "zh StatusBody must not carry a Token line: {zh}"
        );
    }

    #[test]
    fn cp_auth_error_expired_ignores_fallback_and_prompts_relogin() {
        use atomcode_codingplan::AuthExpired;
        let err = anyhow::Error::new(AuthExpired { status: 401 });
        let line = render_cp_auth_error(&err, || "FALLBACK".to_string());
        assert!(
            line.contains("/login"),
            "auth-expired must prompt /login: {line:?}"
        );
        assert!(
            !line.contains("FALLBACK"),
            "expired must not use the fallback: {line:?}"
        );
        // Must NOT bury it as the raw error text.
        assert!(
            !line.contains("authentication failed (401)"),
            "auth-expired should be a clean localized message, not the raw error: {line:?}"
        );
    }

    #[test]
    fn cp_auth_error_non_auth_uses_fallback() {
        // A genuine not-signed-in / network error falls through to the caller's
        // fallback (not-signed-in hint, or the raw fetch-failure line).
        let err = anyhow::anyhow!("network boom");
        let line = render_cp_auth_error(&err, || format!("fetch failed — {err:#}"));
        assert!(
            line.contains("network boom"),
            "non-auth errors fall through to the fallback: {line:?}"
        );
    }
}

#[cfg(test)]
mod rate_limited_tests {
    use super::*;

    // Branch 1: auto_resuming=true → countdown line (WaitAndRetry)
    #[test]
    fn rate_limited_wait_shows_countdown() {
        let line = format_rate_limited_line("", "", Some(45), true, None);
        assert!(line.contains("45"), "should contain countdown seconds");
        assert!(line.contains("自动继续"), "should mention auto-continue");
        assert!(
            line.contains('⏳'),
            "must use clock glyph ⏳ for WaitAndRetry"
        );
        assert!(
            !line.contains('⏸'),
            "must not use pause glyph ⏸ for WaitAndRetry"
        );
    }

    // Branch 2: auto_resuming=false, reset_at_display non-empty → pause with time (Pause)
    #[test]
    fn rate_limited_renders_non_error_pause_line() {
        let line =
            format_rate_limited_line("18:09", "（每 5 小时一个窗口）", Some(7200), false, None);
        assert!(line.contains("18:09"), "should contain reset time");
        assert!(
            line.contains("可换模型") || line.contains("稍后重试"),
            "should contain retry suggestion"
        );
        assert!(!line.starts_with('!'), "must not start with '!' prefix");
        assert!(line.contains('⏸'), "must contain pause glyph ⏸");
        assert!(line.contains("2h0m"), "should format 7200s as 2h0m");
        assert!(!line.contains("自动继续"), "Pause must not say 自动继续");
    }

    // Branch 3: auto_resuming=false, reset_at_display EMPTY → GENERIC 429 (a user's
    // external-model 429, or a gateway 429 with no window data), NOT the CodingPlan
    // "5h window exhausted" message. Locks the mis-attribution fix.
    #[test]
    fn rate_limited_pause_empty_reset_is_generic_not_coding_plan() {
        let line = format_rate_limited_line("", "", None, false, None);
        assert!(line.contains('⏸'), "must use pause glyph ⏸");
        assert!(!line.contains("自动继续"), "must not say 自动继续");
        assert!(
            !line.contains("还有"),
            "must not show countdown when no reset time"
        );
        // The regression guard: an empty-reset 429 must NOT be dressed up as a
        // CodingPlan quota exhaustion.
        assert!(
            !line.contains("5小时窗口"),
            "empty-reset 429 must not claim CodingPlan quota: {line}"
        );
        assert!(
            line.contains("HTTP 429") || line.contains("限流"),
            "should be a generic rate-limit line: {line}"
        );
        assert!(line.contains("稍后重试"), "should indicate to retry later");
    }

    #[test]
    fn rate_limited_generic_surfaces_provider_reason() {
        // A generic (non-CodingPlan) 429 that carried a real provider body — e.g. an
        // external model's "余额不足…请充值" — must surface that actionable reason.
        let line =
            format_rate_limited_line("", "", None, false, Some("余额不足或无可用资源包,请充值"));
        assert!(
            line.contains("余额不足或无可用资源包,请充值"),
            "must show provider reason: {line}"
        );
        assert!(
            line.contains("HTTP 429") || line.contains("限流"),
            "still a generic 429 line: {line}"
        );
        assert!(
            !line.contains("5小时窗口"),
            "must not claim CodingPlan quota: {line}"
        );
    }

    #[test]
    fn rate_limited_coding_plan_ignores_server_message() {
        // A CodingPlan window pause (has reset time) keeps its window message even if a
        // server_message tags along — the reason line is only for the generic branch.
        let line = format_rate_limited_line("18:09", "", Some(7200), false, Some("请充值"));
        assert!(
            line.contains("5小时窗口"),
            "CodingPlan quota keeps its message: {line}"
        );
        assert!(
            !line.contains("请充值"),
            "server_message must not leak into the CodingPlan line: {line}"
        );
    }

    // A gateway CodingPlan quota (real reset time) KEEPS the "5h window" message.
    #[test]
    fn rate_limited_pause_with_reset_time_keeps_coding_plan_message() {
        let line = format_rate_limited_line("18:09", "", Some(7200), false, None);
        assert!(
            line.contains("5小时窗口"),
            "confirmed CodingPlan quota keeps its message: {line}"
        );
        assert!(line.contains("18:09"), "shows the window reset time");
    }

    // Regression (review F2): an exhausted CodingPlan window whose server OMITTED
    // reset_at_display but provided a window LABEL must STILL keep the CodingPlan
    // message — keying on reset_at_display alone would wrongly go generic.
    #[test]
    fn rate_limited_empty_display_but_label_keeps_coding_plan() {
        let line = format_rate_limited_line("", "（每 5 小时一个窗口）", Some(7200), false, None);
        assert!(
            line.contains("5小时窗口"),
            "label alone must keep CodingPlan framing: {line}"
        );
        assert!(
            !line.contains("HTTP 429"),
            "must not fall to the generic line: {line}"
        );
    }

    #[test]
    fn rate_limited_no_secs_shows_no_duration() {
        let line = format_rate_limited_line("23:59", "", None, false, None);
        assert!(line.contains("23:59"));
        assert!(!line.contains("还有"));
    }

    #[test]
    fn rate_limited_pause_no_reset_time_still_shows_remaining_secs() {
        // Pause (auto_resuming=false) with no wall-clock display but a known
        // remaining duration: the duration must NOT be dropped. (Generic 429 line
        // now — no CodingPlan claim without a real reset time.)
        let line = format_rate_limited_line("", "", Some(7200), false, None);
        assert!(line.contains('⏸'), "must use pause glyph");
        assert!(
            !line.contains("自动继续"),
            "must not say auto-continue (this is a Pause)"
        );
        assert!(
            !line.contains("5小时窗口"),
            "empty-reset 429 must not claim CodingPlan quota: {line}"
        );
        assert!(
            line.contains("后可重试"),
            "must surface the remaining duration: {line}"
        );
        assert!(line.contains("2h0m"), "7200s → 2h0m: {line}");
    }

    #[test]
    fn fmt_dur_hours_and_minutes() {
        assert_eq!(fmt_dur(7931), "2h12m"); // 2h 12m 11s → floor minutes
        assert_eq!(fmt_dur(3600), "1h0m");
    }

    #[test]
    fn fmt_dur_minutes_only() {
        assert_eq!(fmt_dur(90), "1m");
        assert_eq!(fmt_dur(120), "2m");
    }

    #[test]
    fn fmt_dur_seconds() {
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(0), "0s");
    }
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
        assert!(!s.contains(URL), "URL must not appear when omit_url:\n{s}");
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
) -> Result<atomcode_auth::AuthInfo> {
    use crossterm::event::KeyCode;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc::error::TryRecvError;

    let session = atomcode_auth::start_login()?;

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
            atomcode_auth::PollOutcome::Authorized => break,
            atomcode_auth::PollOutcome::Pending => {}
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

/// Run `coding_plan::run()` on a blocking thread to prevent
/// `reqwest::blocking::Client`'s internal tokio runtime from being
/// dropped inside the TUI's async context. Returns the mutated config
/// alongside the report — the caller MUST write the returned config back
/// into `ctx.config`.
///
/// See `run_login_flow` for the rationale — the short version is that
/// `reqwest::blocking::Client` creates its own runtime, and dropping it
/// inside an existing runtime panics with "Cannot drop a runtime in a
/// context where blocking is not allowed".
fn run_coding_plan_blocking(
    config: &atomcode_config::config::Config,
    tel: &std::sync::Arc<atomcode_telemetry::Telemetry>,
) -> Result<(
    atomcode_config::config::Config,
    atomcode_codingplan::SetupReport,
)> {
    let mut cfg = config.clone();
    let tel = tel.clone();
    // Run on a dedicated OS thread so `reqwest::blocking::Client`'s
    // internal tokio runtime is created AND dropped outside the TUI's
    // async context. Using `std::thread` instead of
    // `tokio::task::spawn_blocking` keeps the call site synchronous
    // (`run_login_flow` isn't async) and avoids the need to
    // `Handle::block_on`.
    std::thread::spawn(move || {
        let report = atomcode_codingplan::run(&mut cfg, Some(&tel));
        (cfg, report)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("coding plan flow panicked"))
    .and_then(|(cfg, report)| Ok((cfg, report?)))
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
    if !atomcode_auth::is_logged_in() {
        if let Err(e) = run_oauth_with_renderer(renderer, ctx)
            .and_then(|auth| atomcode_auth::save_auth(&auth).map(|_| auth))
        {
            // Login failed/cancelled. Surface as a top-level error;
            // skip the rest of setup since claim/models/status all
            // need a token.
            renderer.render(UiLine::Error(
                t(Msg::CodingPlanSetupFailed {
                    error: &e.to_string(),
                })
                .into_owned(),
            ));
            renderer.flush();
            return Ok(());
        }
    }

    // Phase 2: claim/models/status. Pure HTTP + config mutation — no
    // stdin / stdout interaction, so we don't need to suspend the
    // renderer. `step_login` short-circuits via `is_logged_in()`.
    //
    // CodingPlan's `Client` wraps `reqwest::blocking::Client`, which
    // internally creates its own tokio runtime. Dropping that runtime
    // inside the TUI's async context (where this slash command runs)
    // panics with "Cannot drop a runtime in a context where blocking is
    // not allowed" and `panic = "abort"` kills the process. Run the
    // whole flow on a blocking thread so the internal runtime is created
    // and dropped outside the async context.
    //
    // If the stored token is locally valid (file present, expires_in
    // not yet past) but the server rejects it (revoked, refresh-token
    // dead, etc.), the orchestrator surfaces `report.auth_expired =
    // true`. Run OAuth *once* on that path — same flow `/login` would
    // have used — then re-run setup against the fresh token. Without
    // this the user sees "✓ already logged in as X" followed by
    // "✗ claim failed — run `atomcode login` again" and has to do
    // manually what `/codingplan` could do itself.
    let (mut prepared_config, mut report) =
        match run_coding_plan_blocking(&ctx.config, &ctx.telemetry) {
            Ok((cfg, r)) => (cfg, r),
            Err(e) => {
                renderer.render(UiLine::Error(format!("internal error: {e:#}")));
                renderer.flush();
                return Ok(());
            }
        };
    if report.auth_expired {
        renderer.render(UiLine::CommandOutput(t(Msg::CpReauthAfter401).into_owned()));
        renderer.flush();
        match run_oauth_with_renderer(renderer, ctx)
            .and_then(|auth| atomcode_auth::save_auth(&auth).map(|_| auth))
        {
            Ok(_) => {
                let (cfg_after2, r2) =
                    match run_coding_plan_blocking(&prepared_config, &ctx.telemetry) {
                        Ok((cfg, r)) => (cfg, r),
                        Err(e) => {
                            renderer.render(UiLine::Error(format!("internal error: {e:#}")));
                            renderer.flush();
                            return Ok(());
                        }
                    };
                prepared_config = cfg_after2;
                report = r2;
            }
            Err(e) => {
                // Re-OAuth itself failed (user pressed ESC, network
                // dead, etc.). Render the *original* report so they
                // still see what triggered the retry, then surface the
                // OAuth error.
                renderer.render(UiLine::CommandOutput(report.render()));
                renderer.render(UiLine::Error(
                    t(Msg::CodingPlanSetupFailed {
                        error: &e.to_string(),
                    })
                    .into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
        }
    }

    if report.should_persist_config() {
        // Config mutation only persists when critical steps passed —
        // don't write a half-set-up config if login or models failed.
        match ctx.config_store.update(|latest| {
            atomcode_codingplan::merge_successful_config(latest, &prepared_config, &report)
        }) {
            Ok(commit) => apply_persisted_config(
                ctx,
                commit.snapshot.config,
                commit.snapshot.revision,
                renderer,
            ),
            Err(error) => {
                renderer.render(UiLine::Error(
                    t(Msg::ConfigSaveFailed {
                        error: &error.to_string(),
                    })
                    .into_owned(),
                ));
                renderer.flush();
                return Ok(());
            }
        }
        // Stamp the drift-monitor sync marker alongside the config
        // write. Failures are non-fatal: at worst the 24h staleness
        // hint mis-fires once.
        let _ = atomcode_codingplan::write_last_sync_now();
        // Also bump our own last-seen timestamp so the cross-process
        // sync-check on the next keystroke doesn't redundantly
        // reload the config we just saved ourselves.
        ctx.monitor_last_sync_seen = atomcode_codingplan::read_last_sync();
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
    Ok(())
}

/// The synthetic `todowrite`-empty call + its tool result. Appended to the
/// conversation, they make `reduce_todos`/`derive_current_todos` fold the list to
/// `[]` (the empty list is the last plan), while keeping the transcript's
/// call/result pairing valid for the next request. Kernel (session) message model.
fn todo_clear_messages(id: String) -> Vec<atomcode_kernel::message::Message> {
    use atomcode_kernel::message::Message;
    use atomcode_kernel::tool::ToolCall;
    let mut call = Message::assistant(
        "",
        vec![ToolCall {
            id: id.clone(),
            name: "todowrite".to_string(),
            arguments: r#"{"todos":[]}"#.to_string(),
        }],
    );
    call.internal_origin = Some("todo_clear".to_string());
    let mut result = Message::tool_result(id, "0 tasks", false);
    result.internal_origin = Some("todo_clear".to_string());
    vec![call, result]
}

/// Synthetic tool-call pair for `/todo add <content>`: an incremental
/// `{"action":"add","content":…}` call plus its result. Mirrors
/// [`todo_clear_messages`]; the `content` is JSON-encoded via `serde_json` so
/// quotes/newlines in the user's text can't break the args. Folds through the
/// canonical `reduce_todos` as a new pending task appended at the end.
fn todo_add_messages(id: String, content: &str) -> Vec<atomcode_kernel::message::Message> {
    use atomcode_kernel::message::Message;
    use atomcode_kernel::tool::ToolCall;
    let args = serde_json::json!({ "action": "add", "content": content }).to_string();
    let mut call = Message::assistant(
        "",
        vec![ToolCall {
            id: id.clone(),
            name: "todowrite".to_string(),
            arguments: args,
        }],
    );
    call.internal_origin = Some("todo_add".to_string());
    let mut result = Message::tool_result(id, format!("Added task: {content}"), false);
    result.internal_origin = Some("todo_add".to_string());
    vec![call, result]
}

/// Append a synthetic todo-mutation message `pair` to the conversation and reseed
/// the kernel (the proven `/resume` `SetConversation` path), then rebuild the live
/// panel from the resulting transcript and persist. The subtle reseed dance lives
/// HERE so `/todo add` and `/todo clear` can't drift. The caller supplies the pair
/// (each carries a unique `tool_call_id` — the message count grows by 2 per call,
/// so a constant id would be rejected as a duplicate by a strict gateway). The
/// panel is refolded from the transcript, which naturally yields `None` after a
/// clear (empty `todowrite`) and the appended task after an add — one code path
/// for both.
fn reseed_todo_conversation(
    ctx: &mut LoopCtx,
    state: &mut UiState,
    pair: Vec<atomcode_kernel::message::Message>,
) {
    let mut snapshot = ctx.current_session.to_conversation_snapshot();
    snapshot.messages.extend(pair);
    ctx.runtime
        .dispatch(atomcode_coding::DriverCommand::RestoreSnapshot(
            snapshot.clone(),
        ))
        .ok();
    ctx.current_session
        .update_from_conversation_snapshot(snapshot);
    ctx.current_session.touch();
    state.active_todos =
        crate::event_loop::todo_progress_from_messages(&ctx.current_session.messages);
    crate::event_loop::sync_todo_titles(state);
}

/// `/todo add <content>` — deterministically append a pending task without waiting
/// on the model, so the next turn's TodoHook sees it and the model can act on it.
fn add_todo(ctx: &mut LoopCtx, state: &mut UiState, content: &str) {
    let id = format!("todo-add-{}", ctx.current_session.messages.len());
    reseed_todo_conversation(ctx, state, todo_add_messages(id, content));
}

/// `/todo clear` — deterministically wipe the task list without waiting on the
/// model, so cancelled/stale tasks stop reappearing (the next turn derives an
/// empty list and injects nothing).
fn clear_todos(ctx: &mut LoopCtx, state: &mut UiState) {
    let id = format!("todo-clear-{}", ctx.current_session.messages.len());
    reseed_todo_conversation(ctx, state, todo_clear_messages(id));
}

/// Build the `/todo` output from the session message history.
///
/// Scans the transcript backwards for the most recent `todowrite` tool call,
/// parses its `todos` array, and renders one line per task.  Returns a
/// "no list" message when no such call has been made yet.
///
/// Pure function — no I/O, no side effects.  Easy to unit-test in isolation.
pub(crate) fn format_todo_command(
    messages: &[atomcode_kernel::message::Message],
    unicode: bool,
) -> String {
    // Match the runtime hook, daemon command, and resume panel: failed calls do
    // not become current state, while successful legacy calls remain readable.
    let todos = atomcode_capabilities::tools::todo::derive_current_todos(messages);
    if todos.is_empty() {
        return t(Msg::TodoNoList).into_owned();
    }
    format!(
        "{}\n{}",
        t(Msg::TodoListHeader),
        atomcode_capabilities::tools::todo::render_todos_text(&todos, unicode)
    )
}

#[cfg(test)]
mod copy_tests {
    use super::{extract_code_blocks, resolve_copy, CopyResolve};

    const REPLY: &str = "Run cmake + build:\n\
        ```\n\
        cmake D:\\proj -DBUILD=ON -DLONG=\"a very long windows path here\"\n\
        ```\n\
        then:\n\
        ```bash\n\
        cmake --build . --target demo -j4\n\
        ```";

    #[test]
    fn extracts_blocks_verbatim_in_order() {
        let blocks = extract_code_blocks(REPLY);
        assert_eq!(blocks.len(), 2);
        // No hard-wrap, no PAD indent — the command is one logical line.
        assert_eq!(
            blocks[0],
            "cmake D:\\proj -DBUILD=ON -DLONG=\"a very long windows path here\""
        );
        assert_eq!(blocks[1], "cmake --build . --target demo -j4");
    }

    #[test]
    fn multiline_block_preserves_inner_newlines_and_indent() {
        let md = "```\nline1\n  indented2\nline3\n```";
        let blocks = extract_code_blocks(md);
        assert_eq!(blocks, vec!["line1\n  indented2\nline3".to_string()]);
    }

    #[test]
    fn unterminated_fence_still_yields_partial() {
        // A reply truncated mid-stream — still copyable.
        let md = "```\nhalf a command";
        assert_eq!(extract_code_blocks(md), vec!["half a command".to_string()]);
    }

    #[test]
    fn no_fence_yields_nothing() {
        assert!(extract_code_blocks("just prose, `inline code` only").is_empty());
    }

    #[test]
    fn longer_fence_can_contain_a_shorter_fence() {
        let md = "````markdown\n```rust\nfn main() {}\n```\n````";
        assert_eq!(
            extract_code_blocks(md),
            vec!["```rust\nfn main() {}\n```".to_string()]
        );
    }

    #[test]
    fn tilde_fence_requires_a_matching_marker() {
        let md = "~~~text\n```\nstill inside\n~~~";
        assert_eq!(
            extract_code_blocks(md),
            vec!["```\nstill inside".to_string()]
        );
    }

    #[test]
    fn resolve_default_picks_last_block() {
        match resolve_copy(REPLY, "") {
            CopyResolve::Text(t, _) => assert_eq!(t, "cmake --build . --target demo -j4"),
            _ => panic!("default should resolve to the last block"),
        }
    }

    #[test]
    fn resolve_index_is_one_based() {
        match resolve_copy(REPLY, "1") {
            CopyResolve::Text(t, _) => assert!(t.starts_with("cmake D:\\proj")),
            _ => panic!("/copy 1 should pick the first block"),
        }
    }

    #[test]
    fn resolve_all_joins_every_block() {
        match resolve_copy(REPLY, "all") {
            CopyResolve::Text(t, _) => {
                assert!(t.contains("-DBUILD=ON"));
                assert!(t.contains("--build ."));
            }
            _ => panic!("/copy all should join blocks"),
        }
    }

    #[test]
    fn resolve_bad_index_reports_count() {
        assert!(matches!(resolve_copy(REPLY, "9"), CopyResolve::BadIndex(2)));
        assert!(matches!(resolve_copy(REPLY, "0"), CopyResolve::BadIndex(2)));
        assert!(matches!(resolve_copy(REPLY, "x"), CopyResolve::BadIndex(2)));
    }

    #[test]
    fn resolve_no_blocks_when_reply_has_none() {
        assert!(matches!(
            resolve_copy("plain reply", ""),
            CopyResolve::NoBlocks
        ));
        assert!(matches!(resolve_copy("", ""), CopyResolve::NoBlocks));
    }
}

#[cfg(test)]
mod save_tests {
    use super::{
        default_save_filename, expand_tilde_path, render_save_markdown, resolve_save_in,
        SaveOutcome,
    };
    use atomcode_kernel::message::{Message, Role};
    use std::path::{Path, PathBuf};

    /// Build a kernel text message with an explicit role (kernel has no generic
    /// `new(role, text)`).
    fn msg(role: Role, text: &str) -> Message {
        let mut m = Message::user(text);
        m.role = role;
        m
    }

    #[test]
    fn expand_tilde_path_maps_home_prefix() {
        let home = PathBuf::from("/home/u");
        assert_eq!(expand_tilde_path("~", Some(&home)), home);
        assert_eq!(
            expand_tilde_path("~/notes.md", Some(&home)),
            home.join("notes.md")
        );
        // Not a home-relative path → unchanged.
        assert_eq!(
            expand_tilde_path("report.md", Some(&home)),
            PathBuf::from("report.md")
        );
        assert_eq!(
            expand_tilde_path("/abs/x.md", Some(&home)),
            PathBuf::from("/abs/x.md")
        );
        // `~user` is NOT expanded (we don't resolve other users' homes).
        assert_eq!(
            expand_tilde_path("~bob/x", Some(&home)),
            PathBuf::from("~bob/x")
        );
        // No home known → passthrough (never fabricate a path).
        assert_eq!(expand_tilde_path("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn save_refuses_to_overwrite_existing_non_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("config.py");
        std::fs::write(&target, "SECRET = 1\n").unwrap();
        let msgs = vec![msg(Role::User, "hi")];
        match resolve_save_in(&msgs, target.to_str().unwrap(), dir.path()) {
            SaveOutcome::RefuseOverwrite(p) => assert!(p.contains("config.py"), "{p}"),
            other => panic!("expected RefuseOverwrite, got {other:?}"),
        }
        // The existing file must be untouched.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "SECRET = 1\n");
    }

    #[test]
    fn save_overwrites_existing_markdown_and_allows_new_nonmd() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = vec![msg(Role::User, "hi")];
        // Existing .md → overwrite is fine (re-export).
        let md = dir.path().join("report.md");
        std::fs::write(&md, "old").unwrap();
        assert!(matches!(
            resolve_save_in(&msgs, md.to_str().unwrap(), dir.path()),
            SaveOutcome::Ok(_)
        ));
        assert!(std::fs::read_to_string(&md).unwrap().contains("## User"));
        // A NEW non-md file (no clobber) → allowed.
        let fresh = dir.path().join("notes");
        assert!(matches!(
            resolve_save_in(&msgs, fresh.to_str().unwrap(), dir.path()),
            SaveOutcome::Ok(_)
        ));
        assert!(Path::new(&fresh).is_file());
    }

    /// Build a Vec<Message> from (role, text) pairs for test fixtures.
    fn conv(msgs: &[(&str, &str)]) -> Vec<Message> {
        msgs.iter()
            .map(|(role, text)| {
                msg(
                    match *role {
                        "user" => Role::User,
                        "assistant" => Role::Assistant,
                        _ => Role::System,
                    },
                    text,
                )
            })
            .collect()
    }

    #[test]
    fn save_empty_history_when_no_messages() {
        // Empty history short-circuits before any path work, so working_dir is unused.
        assert!(matches!(
            resolve_save_in(&[], "", Path::new(".")),
            SaveOutcome::EmptyHistory
        ));
    }

    #[test]
    fn save_empty_history_when_only_tool_messages() {
        let msgs = vec![msg(Role::Tool, "tool output")];
        assert!(matches!(
            resolve_save_in(&msgs, "", Path::new(".")),
            SaveOutcome::EmptyHistory
        ));
    }

    #[test]
    fn save_empty_history_when_only_whitespace() {
        let msgs = conv(&[("user", "   "), ("assistant", "\n  \t")]);
        assert!(matches!(
            resolve_save_in(&msgs, "", Path::new(".")),
            SaveOutcome::EmptyHistory
        ));
    }

    #[test]
    fn save_default_filename_format() {
        // Pure naming check — no I/O, safe to run in parallel.
        let name = default_save_filename();
        assert!(name.starts_with("atomcode-session-"), "got: {name}");
        assert!(name.ends_with(".md"), "got: {name}");
        // atomcode-session-YYYYMMDD-HHMMSS.md → 17 + 15 + 3 = 35 chars
        assert_eq!(name.len(), "atomcode-session-YYYYMMDD-HHMMSS.md".len());
    }

    #[test]
    fn save_render_markdown_formats_turns() {
        let msgs = conv(&[("user", "hello"), ("assistant", "hi there")]);
        let md = render_save_markdown(&msgs).expect("non-empty renders");
        assert!(md.starts_with("# AtomCode Session - "));
        assert!(md.contains("## User\nhello\n\n"));
        assert!(md.contains("## Assistant\nhi there\n\n"));
    }

    #[test]
    fn save_render_skips_synthetic_and_tool_messages() {
        let msgs = vec![
            msg(Role::User, "real prompt"),
            Message::synthetic_user("synthetic injection"),
            msg(Role::Tool, "tool noise"),
            msg(Role::Assistant, "reply"),
        ];
        let md = render_save_markdown(&msgs).expect("renders");
        assert!(md.contains("## User\nreal prompt"));
        assert!(md.contains("## Assistant\nreply"));
        assert!(!md.contains("synthetic injection"));
        assert!(!md.contains("tool noise"));
    }

    #[test]
    fn save_render_returns_none_for_empty() {
        assert!(render_save_markdown(&[]).is_none());
        assert!(render_save_markdown(&[msg(Role::Tool, "x")]).is_none());
    }

    #[test]
    fn save_writes_relative_path_in_active_working_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let msgs = conv(&[("user", "ping"), ("assistant", "pong")]);

        match resolve_save_in(&msgs, "session.md", tmp.path()) {
            SaveOutcome::Ok(got) => {
                assert_eq!(got, tmp.path().join("session.md").canonicalize().unwrap());
                assert!(got.is_file());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn save_writes_custom_absolute_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("report.md");
        let msgs = conv(&[("user", "ping"), ("assistant", "pong")]);
        match resolve_save_in(&msgs, path.to_str().unwrap(), tmp.path()) {
            SaveOutcome::Ok(got) => {
                // canonicalize() may add a platform-specific prefix
                // (e.g. \\?\ on Windows), so compare by file name + read-back
                // rather than exact path equality.
                assert_eq!(got.file_name(), path.file_name());
                let content = std::fs::read_to_string(&got).expect("read");
                assert!(content.contains("## User\nping"));
                assert!(content.contains("## Assistant\npong"));
            }
            _ => panic!(
                "expected Ok, got {:?}",
                resolve_save_in(&msgs, path.to_str().unwrap(), tmp.path())
            ),
        }
    }

    #[test]
    fn save_overwrites_existing_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("old.md");
        std::fs::write(&path, "OLD CONTENT").expect("seed");
        let msgs = conv(&[("user", "new turn")]);
        match resolve_save_in(&msgs, path.to_str().unwrap(), tmp.path()) {
            SaveOutcome::Ok(got) => {
                assert_eq!(got.file_name(), path.file_name());
                let content = std::fs::read_to_string(&got).expect("read");
                assert!(content.contains("## User\nnew turn"));
                assert!(!content.contains("OLD CONTENT"));
            }
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn save_invalid_path_when_parent_dir_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent_dir").join("out.md");
        let msgs = conv(&[("user", "hi")]);
        match resolve_save_in(&msgs, path.to_str().unwrap(), tmp.path()) {
            SaveOutcome::InvalidPath(p) => assert!(p.contains("nonexistent_dir"), "got: {p}"),
            other => panic!("expected InvalidPath, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod expand_cd_target_tests {
    use super::expand_cd_target;
    use std::path::{Path, PathBuf};

    #[test]
    fn tilde_accepts_forward_and_back_slash() {
        let home = PathBuf::from("/home/u");
        let cwd = PathBuf::from("/work");
        // `~/Desktop` and `~\Desktop` (Windows) must both expand to <home>/Desktop.
        assert_eq!(
            expand_cd_target("~/Desktop", Some(&home), &cwd, None).unwrap(),
            home.join("Desktop")
        );
        assert_eq!(
            expand_cd_target("~\\Desktop", Some(&home), &cwd, None).unwrap(),
            home.join("Desktop")
        );
        assert_eq!(
            expand_cd_target("~", Some(&home), &cwd, None).unwrap(),
            home
        );
    }

    #[test]
    fn tilde_strips_all_leading_separators_no_home_escape() {
        // `~//Desktop` / `~\\Desktop` (double separator, easy typo) must stay
        // home-relative — NOT degrade to the absolute `/Desktop` that a single
        // `strip_prefix` would leave (Path::join with an absolute arg drops home).
        let home = PathBuf::from("/home/u");
        let cwd = PathBuf::from("/work");
        assert_eq!(
            expand_cd_target("~//Desktop", Some(&home), &cwd, None).unwrap(),
            home.join("Desktop")
        );
        assert_eq!(
            expand_cd_target("~\\\\Desktop", Some(&home), &cwd, None).unwrap(),
            home.join("Desktop")
        );
    }

    #[test]
    fn relative_joins_cwd_absolute_kept() {
        let cwd = PathBuf::from("/work");
        assert_eq!(
            expand_cd_target("sub", None, &cwd, None).unwrap(),
            cwd.join("sub")
        );
        assert_eq!(
            expand_cd_target("/abs/path", None, &cwd, None).unwrap(),
            Path::new("/abs/path")
        );
    }

    #[test]
    fn dash_uses_previous_dir() {
        let cwd = PathBuf::from("/work");
        let prev = PathBuf::from("/old");
        assert_eq!(
            expand_cd_target("-", None, &cwd, Some(&prev)).unwrap(),
            prev
        );
        assert!(expand_cd_target("-", None, &cwd, None).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_user_text_shaped_like_a_reminder_is_still_echoed() {
        let input = atomcode_coding::UserInput {
            text: atomcode_capabilities::reminder::system_reminder(
                "Explain why this appears.",
            ),
            images: Vec::new(),
        };

        assert!(matches!(
            project_live_view_event(
                atomcode_daemon::live_hub::LiveViewEvent::InputAccepted {
                    input,
                    client_input_id: Some("web-input".into()),
                }
            ),
            Some(crate::event_loop::ui_event::UiEvent::UserEcho(text))
                if text == atomcode_capabilities::reminder::system_reminder(
                    "Explain why this appears."
                )
        ));
    }

    #[test]
    fn live_user_text_that_only_mentions_reminder_tag_is_still_echoed() {
        let input = atomcode_coding::UserInput {
            text: "Why is <system-reminder> visible?".into(),
            images: Vec::new(),
        };

        assert!(matches!(
            project_live_view_event(
                atomcode_daemon::live_hub::LiveViewEvent::InputAccepted {
                    input,
                    client_input_id: Some("web-input".into()),
                }
            ),
            Some(crate::event_loop::ui_event::UiEvent::UserEcho(text))
                if text == "Why is <system-reminder> visible?"
        ));
    }

    #[test]
    fn live_request_resolution_projects_to_correlated_tui_event() {
        let event =
            project_live_view_event(atomcode_daemon::live_hub::LiveViewEvent::RequestResolved {
                request_id: 42,
                kind: atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND
                    .into(),
            })
            .expect("request terminal must reach the TUI");

        assert!(matches!(
            event,
            crate::event_loop::ui_event::UiEvent::SharedRequestResolved {
                request_id: 42,
                ref kind,
            } if kind
                == atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND
        ));
    }

    fn new_schema_config(default_model: Option<&str>) -> Config {
        serde_json::from_value(serde_json::json!({
            "default_provider": "",
            "default_model": default_model,
            "provider_accounts": {
                "AtomGit": {
                    "provider": "openai",
                    "base_url": "https://llm-api.atomgit.com/v1"
                }
            },
            "models": {
                "AtomGit-Qwen": {
                    "account": "AtomGit",
                    "model": "Qwen3-VL-8B-Instruct",
                    "context_window": 131072
                }
            }
        }))
        .unwrap()
    }

    #[test]
    fn live_provider_selection_uses_default_model_when_legacy_default_is_empty() {
        let config = new_schema_config(Some("AtomGit-Qwen"));
        assert_eq!(live_provider_selection(&config).unwrap(), "AtomGit-Qwen");
    }

    #[test]
    fn live_provider_selection_matches_runtime_catalog_fallback() {
        let config = new_schema_config(None);
        assert_eq!(live_provider_selection(&config).unwrap(), "AtomGit-Qwen");
    }

    #[test]
    fn live_provider_selection_reports_missing_catalog_without_empty_provider_error() {
        let config = Config::default();
        let error = live_provider_selection(&config).unwrap_err();
        assert!(error.contains("no model is configured"));
        assert!(!error.contains("provider \"\" not found"));
    }

    #[test]
    fn review_prompt_uses_explicit_tool_scopes() {
        assert!(review_prompt("").contains(
            r#"{"scope":{"kind":"working_tree"}}"#
        ));
        assert!(review_prompt("staged").contains(
            r#"{"scope":{"kind":"staged"}}"#
        ));
        let range = review_prompt("release/v5.0.5");
        assert!(range.contains(
            r#"{"scope":{"kind":"range","base":"release/v5.0.5","head":"HEAD"}}"#
        ));
        assert!(!range.contains(r#"{"base":"#));
    }

    #[test]
    fn review_prompt_json_escapes_the_base_ref() {
        let prompt = review_prompt("odd\"ref");
        assert!(prompt.contains(r#""base":"odd\"ref""#));
        assert!(!prompt.contains("`odd\"ref..HEAD`"));
    }

    #[test]
    fn context_file_status_shows_instruction_and_memory_paths() {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "project instructions").unwrap();
        let project_memory = MemoryStore::project(project.path());
        std::fs::create_dir_all(project_memory.path().parent().unwrap()).unwrap();
        std::fs::write(project_memory.path(), "- remembered fact\n").unwrap();
        let status = render_context_file_status_block(project.path());
        assert!(status.contains(&project.path().join("AGENTS.md").display().to_string()));
        assert!(status.contains(&project_memory.path().display().to_string()));
        assert!(status.contains("(PROJECT)") || status.contains("（PROJECT）"));
        assert!(
            status.contains("Instruction files") || status.contains("指令文件"),
            "instruction section should be visible: {status}"
        );
        assert!(
            status.contains("Memory files") || status.contains("记忆文件"),
            "memory section should be visible: {status}"
        );
    }

    #[test]
    fn streaming_usage_snapshot_composes_plan_and_window_lines() {
        use atomcode_codingplan::types::{PlanInfo, RateLimitWindow};
        // Build fixtures from JSON (serde defaults fill the fields we don't care about).
        let plan: PlanInfo = serde_json::from_value(serde_json::json!({
            "plan_name": "AtomPlan-Pro", "expires_at": "2026-12-31",
            "remaining_days": 30, "total_days": 365
        }))
        .unwrap();
        let window: RateLimitWindow = serde_json::from_value(serde_json::json!({
            "usage_status_desc": "42% used", "reset_at_display": "12:00",
            "usage_percent": 42.0, "seconds_until_reset": 3600,
            "window_hours": 5, "show_enable": 1
        }))
        .unwrap();
        let data = UsageData {
            window: Some(window),
            plan: Some(plan),
            usage: None,
            overview: None,
            error: None,
        };
        // The streaming footer report renders the active (default: Current) tab.
        let text = UsageModal::new(data).active_snapshot_text(true, true);
        assert!(text.contains("AtomPlan-Pro"), "plan name present: {text}");
        assert!(text.contains("42.0%"), "window progress present: {text}");
        assert!(
            text.contains("\x1b[32m") && text.contains("\x1b[1m"),
            "streaming snapshot must preserve modal colors and emphasis: {text:?}"
        );

        // Logged in but empty gateway response → still non-blank (tab bar + the
        // Current tab's "unavailable" body), never a bare footer.
        let empty = UsageData {
            window: None,
            plan: None,
            usage: None,
            overview: None,
            error: None,
        };
        assert!(
            !UsageModal::new(empty)
                .active_snapshot_text(true, true)
                .is_empty(),
            "empty data must not render blank"
        );
    }

    #[test]
    fn active_session_bucket_uses_runtime_directory_not_embedded_metadata() {
        let runtime_dir = PathBuf::from("/current/project");
        let stale_meta_dir = PathBuf::from("/old/project");

        let bucket = active_session_project_bucket(&runtime_dir);

        assert_eq!(
            bucket,
            atomcode_capabilities::session::SessionManager::project_hash(&runtime_dir)
        );
        assert_ne!(
            bucket,
            atomcode_capabilities::session::SessionManager::project_hash(&stale_meta_dir)
        );
    }

    /// Create a subdir inside a tempdir and return both. Paths are
    /// canonicalized because `resolve_cd` canonicalizes its output, and
    /// on macOS `/var/folders/...` → `/private/var/folders/...`.
    ///
    /// The verbatim prefix is stripped to match `resolve_cd`'s new contract:
    /// on Windows `canonicalize` yields `\\?\C:\…`, but `resolve_cd` strips that
    /// at the source, so the expected values here must strip too or every
    /// comparison below would fail on Windows. No-op off Windows.
    fn make_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let strip = atomcode_capabilities::pathnorm::strip_verbatim_path;
        let cwd = strip(&tmp.path().canonicalize().expect("canon cwd"));
        let sub = cwd.join("sub");
        std::fs::create_dir(&sub).expect("mkdir sub");
        let sub = strip(&sub.canonicalize().expect("canon sub"));
        (tmp, cwd, sub)
    }

    /// `resolve_cd` must never return a Windows `\\?\` verbatim / extended-length
    /// path — that raw form leaked into the `/cd` confirmation message and the
    /// webui footer chip (`\\?\C:\Users\hao\atomcode`). Trivially true off
    /// Windows; the real guard is on Windows, where `canonicalize` adds the prefix.
    // The picker showed the same dir twice (`~/atomcode` ×2) because
    // recent_dirs.txt accumulated BOTH the `\\?\C:\…` verbatim form and the plain
    // `C:\…` form of one dir. Stripping collapses them; parse must then de-dup so
    // the picker shows each dir once.
    #[test]
    fn parse_recent_dirs_strips_verbatim_and_dedups() {
        let contents = format!(
            "{}\n{}\n{}\n",
            r"\\?\C:\Users\hao\atomcode", // legacy verbatim form
            r"C:\Users\hao\atomcode",     // plain form of the SAME dir
            r"C:\Users\hao\temp0620",
        );
        assert_eq!(
            parse_recent_dirs(&contents),
            vec![
                PathBuf::from(r"C:\Users\hao\atomcode"),
                PathBuf::from(r"C:\Users\hao\temp0620"),
            ],
            "verbatim + plain forms of one dir must collapse to a single entry"
        );
        // Blank lines skipped; exact dupes collapsed; first-occurrence order kept.
        assert_eq!(
            parse_recent_dirs("/a\n\n/b\n/a\n"),
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
        );
    }

    // On case-insensitive filesystems (Windows/macOS) the SAME dir written in two
    // cases (a launcher passing C:\users vs C:\Users) must collapse to one entry;
    // first occurrence wins. See the reported bug: recent_dirs.txt held both.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn parse_recent_dirs_collapses_case_variants_on_case_insensitive_fs() {
        assert_eq!(
            parse_recent_dirs("/Users/danan\n/users/danan\n"),
            vec![PathBuf::from("/Users/danan")],
            "same dir in two cases must collapse, keeping the first"
        );
    }

    #[test]
    fn cd_picker_dirs_merge_current_recent_and_complete_catalog() {
        let current = PathBuf::from("/current");
        let recent = vec![PathBuf::from("/recent"), PathBuf::from("/current")];
        let catalog = vec![
            PathBuf::from("/catalog-new"),
            PathBuf::from("/recent"),
            PathBuf::from("/catalog-old"),
        ];
        let dirs = merge_cd_picker_dirs(&current, &recent, catalog, |_| true);
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/current"),
                PathBuf::from("/catalog-new"),
                PathBuf::from("/recent"),
                PathBuf::from("/catalog-old"),
            ]
        );
    }

    #[test]
    fn cd_picker_dirs_drop_missing_catalog_projects_without_truncating() {
        let catalog = (0..12).map(|n| PathBuf::from(format!("/project-{n}")));
        let dirs = merge_cd_picker_dirs(Path::new("/current"), &[], catalog, |path| {
            path != Path::new("/project-5")
        });
        assert_eq!(dirs.len(), 12, "current plus 11 live catalog projects");
        assert!(!dirs.contains(&PathBuf::from("/project-5")));
        assert!(dirs.contains(&PathBuf::from("/project-11")));
    }

    #[test]
    fn cd_picker_checks_each_catalog_directory_once() {
        let checks = std::cell::Cell::new(0usize);
        let catalog = vec![
            PathBuf::from("/same"),
            PathBuf::from("/same"),
            PathBuf::from("/other"),
        ];
        let _ = merge_cd_picker_dirs(Path::new("/same"), &[], catalog, |_| {
            checks.set(checks.get() + 1);
            true
        });
        assert_eq!(checks.get(), 2, "one liveness check per unique catalog dir");
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn push_recent_dir_dedups_case_variants_on_case_insensitive_fs() {
        let mut dirs = vec![PathBuf::from("/Users/danan"), PathBuf::from("/other")];
        push_recent_dir(&mut dirs, PathBuf::from("/users/danan"));
        assert_eq!(
            dirs,
            vec![PathBuf::from("/users/danan"), PathBuf::from("/other")],
            "re-pushing the same dir in a different case moves it to front, no dupe"
        );
    }

    #[test]
    fn resolve_cd_strips_verbatim_prefix() {
        let (_tmp, cwd, _sub) = make_dirs();
        let got = resolve_cd(".", &cwd, None).expect("cwd resolves");
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "resolve_cd leaked a verbatim prefix: {}",
            got.display()
        );
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
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
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
        // `resolve_cd` strips the Windows `\\?\` verbatim prefix, so strip the
        // expected value to match (no-op off Windows).
        let canon_home = atomcode_capabilities::pathnorm::strip_verbatim_path(&canon_home);
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

    // ── /copy msg ────────────────────────────────────────────────────
    // `/copy msg` copies the full reply markdown (prose + code), not just
    // the fenced code blocks. This is useful for pasting the whole answer
    // into another document or chat.

    #[test]
    fn copy_msg_returns_full_markdown_when_reply_has_prose_and_code() {
        let md = "Here is the plan:\n\n```rust\nfn main() {}\n```\n\nDone.";
        match resolve_copy(md, "msg") {
            CopyResolve::Text(s, is_msg) => {
                assert_eq!(s, md);
                assert!(is_msg, "/copy msg should flag the result so the caller shows the reply confirmation, not the code-block one");
            }
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn copy_msg_returns_prose_only_reply_without_code_blocks() {
        // A reply with no fenced code block still has a meaningful body.
        // `/copy msg` should return it; `/copy` (no arg) would return NoBlocks.
        let md = "Just a plain explanation with no code.";
        match resolve_copy(md, "msg") {
            CopyResolve::Text(s, is_msg) => {
                assert_eq!(s, md);
                assert!(is_msg, "/copy msg should flag the result so the caller shows the reply confirmation, not the code-block one");
            }
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn copy_msg_trims_leading_trailing_whitespace() {
        let md = "\n\n  Hello world  \n\n";
        match resolve_copy(md, "msg") {
            CopyResolve::Text(s, is_msg) => {
                assert_eq!(s, "Hello world");
                assert!(is_msg);
            }
            other => panic!("expected Text, got {:?}", other),
        }
    }

    #[test]
    fn copy_msg_returns_empty_msg_when_reply_is_empty() {
        // Empty/whitespace-only reply: nothing meaningful to copy.
        // Distinct from NoBlocks so the caller can show a "reply is empty"
        // hint instead of the misleading "no code block" wording.
        for empty in ["", "   ", "\n\n"] {
            match resolve_copy(empty, "msg") {
                CopyResolve::EmptyMsg => {}
                other => panic!("expected EmptyMsg for {:?}, got {:?}", empty, other),
            }
        }
    }

    #[test]
    fn copy_msg_is_case_insensitive() {
        let md = "Some text.";
        for variant in ["msg", "MSG", "Msg", "mSg"] {
            match resolve_copy(md, variant) {
                CopyResolve::Text(_, is_msg) => {
                    assert!(is_msg, "case {:?} should flag is_msg", variant)
                }
                other => panic!("case {:?} should match, got {:?}", variant, other),
            }
        }
    }

    #[test]
    fn copy_msg_does_not_break_existing_copy_no_arg() {
        // Regression: `/copy` (no arg) still returns last code block.
        let md = "intro\n```js\na()\n```\n```py\nb()\n```";
        match resolve_copy(md, "") {
            CopyResolve::Text(s, _) => assert_eq!(s, "b()"),
            other => panic!("expected last block, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod memory_command_tests {
    #[test]
    fn remember_project_writes_directly_to_store() {
        use atomcode_capabilities::memory::MemoryStore;
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::project(tmp.path());
        // 迁移后 /remember 走 MemoryStore::project(cwd).append —— 这里直接验证 store 语义,
        // 命令臂在 Step 4 改为调用它。
        store.append("uses tabs not spaces").unwrap();
        let entries = MemoryStore::project(tmp.path()).load();
        assert!(entries.iter().any(|e| e == "uses tabs not spaces"));
    }
}

#[cfg(test)]
mod todo_command_tests {
    use super::{
        decide_custom_command, format_todo_command, render_custom_command_error, CustomDispatch,
    };
    use crate::custom_commands::ArgsRequirement;
    use crate::render::{Renderer, UiLine};
    use atomcode_config::i18n::{t, Msg};
    use atomcode_kernel::message::{Message, Role};
    use atomcode_kernel::tool::ToolCall;
    use std::path::PathBuf;

    /// Kernel text message with an explicit role.
    fn msg(role: Role, text: &str) -> Message {
        let mut m = Message::user(text);
        m.role = role;
        m
    }

    /// Assistant message carrying `tool_calls` (kernel flat field).
    fn tool_call_msg(calls: Vec<ToolCall>) -> Message {
        Message::assistant("", calls)
    }

    #[test]
    fn todo_command_text_with_and_without_list() {
        // No todowrite calls → "no list" message (i18n'd).
        let empty = vec![msg(Role::User, "hi")];
        let no_list = t(Msg::TodoNoList).into_owned();
        assert!(
            format_todo_command(&empty, false).contains(&no_list),
            "empty messages should contain the i18n no-list message: {no_list:?}"
        );

        // A todowrite call with one pending item → list output.
        let with = vec![tool_call_msg(vec![ToolCall {
            id: "1".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"do x","status":"pending"}]}"#.into(),
        }])];
        let out = format_todo_command(&with, false);
        assert!(
            out.contains("[ ] do x"),
            "expected '[ ] do x' in output, got:\n{out}"
        );
    }

    #[test]
    fn custom_required_args_render_and_metadata() {
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Required,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let rendered = custom.render("myreview", "foo");
        assert_eq!(rendered, Some("review foo".into()));
        let rendered_empty = custom.render("myreview", "");
        assert_eq!(rendered_empty, Some("review ".into()));
        // Required + empty → render still works (template subst is value-neutral),
        // the validation lives in the dispatch layer.
        let cmd = custom.resolve("myreview").unwrap();
        assert_eq!(cmd.args_requirement, ArgsRequirement::Required);
    }

    #[test]
    fn dispatch_custom_optional_empty_arg_submits() {
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Optional,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let rendered = custom.render("myreview", "");
        assert_eq!(rendered, Some("review ".into()));
        let cmd = custom.resolve("myreview").unwrap();
        assert_eq!(cmd.args_requirement, ArgsRequirement::Optional);
    }

    #[test]
    fn dispatch_custom_none_empty_arg_submits() {
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::None,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let rendered = custom.render("myreview", "");
        assert_eq!(rendered, Some("review ".into()));
        let cmd = custom.resolve("myreview").unwrap();
        assert_eq!(cmd.args_requirement, ArgsRequirement::None);
    }

    // ── dispatch decision: `Required` + empty arg ⇒ Reject ──────────────
    //
    // Regression guard for the `other` arm of `execute_slash_command_impl`.
    // The prior tests above only covered `CustomCommandRegistry::render`'s
    // template substitution and the `args_requirement` metadata; none
    // exercised the dispatcher's reject-vs-submit boundary. If a future
    // refactor drops the `CmdCustomArgRequired` arm or flips the rule so
    // `Required` + empty silently submits an empty template, the existing
    // suite would not catch it — these do.

    #[test]
    fn dispatch_required_empty_arg_is_rejected() {
        // Trigger: user typed `/myreview` (Required) and pressed Enter
        // with no argument. Expected: render CmdCustomArgRequired, do
        // NOT submit an agent turn.
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Required,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let decision = decide_custom_command(&custom, "myreview", "");
        assert!(
            matches!(decision, CustomDispatch::Reject),
            "Required command with empty arg must be rejected, got {:?}",
            decision
        );
    }

    #[test]
    fn todo_clear_pair_folds_the_list_to_empty() {
        // A live plan, then the `/todo clear` synthetic pair appended, must
        // derive to an empty list → `/todo` shows the "no list" message. This
        // is what makes cancelled/stale tasks stop reappearing.
        let mut msgs = vec![tool_call_msg(vec![ToolCall {
            id: "1".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"do x","status":"pending"}]}"#.into(),
        }])];
        assert!(format_todo_command(&msgs, false).contains("[ ] do x"));
        msgs.extend(super::todo_clear_messages("todo-clear-1".to_string()));
        let no_list = t(Msg::TodoNoList).into_owned();
        assert!(
            format_todo_command(&msgs, false).contains(&no_list),
            "after the clear pair, /todo must show the no-list message; got:\n{}",
            format_todo_command(&msgs, false)
        );
    }

    #[test]
    fn dispatch_required_whitespace_only_arg_is_rejected() {
        // The dispatcher trims before checking emptiness, so a bare
        // space/tab arg should also reject — otherwise the user could
        // bypass validation by typing `/myreview ` (trailing space gets
        // injected by the needs_args menu auto-complete path).
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Required,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let decision = decide_custom_command(&custom, "myreview", "   \t  ");
        assert!(
            matches!(decision, CustomDispatch::Reject),
            "Required command with whitespace-only arg must be rejected, got {:?}",
            decision
        );
    }

    #[test]
    fn todo_add_pair_appends_a_task_keeping_existing() {
        // A live plan, then the `/todo add` synthetic pair appended, must fold to
        // the ORIGINAL task plus the new one at the end — existing tasks untouched.
        let mut msgs = vec![tool_call_msg(vec![ToolCall {
            id: "1".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"do x","status":"in_progress"}]}"#.into(),
        }])];
        msgs.extend(super::todo_add_messages(
            "todo-add-1".to_string(),
            "ship it",
        ));
        let out = format_todo_command(&msgs, false);
        assert!(out.contains("do x"), "existing task must remain:\n{out}");
        assert!(
            out.contains("[ ] ship it"),
            "new pending task appended:\n{out}"
        );
    }

    #[test]
    fn todo_add_from_empty_creates_the_list() {
        // No prior plan: `/todo add` alone should create a one-item list.
        let msgs = super::todo_add_messages("todo-add-0".to_string(), "first task");
        let out = format_todo_command(&msgs, false);
        assert!(
            out.contains("[ ] first task"),
            "add-from-empty seeds the list:\n{out}"
        );
    }

    #[test]
    fn todo_add_content_with_quotes_is_json_safe() {
        // serde_json encoding must keep the args valid so the fold sees the task.
        let msgs = super::todo_add_messages("todo-add-2".to_string(), r#"handle "weird" input"#);
        let out = format_todo_command(&msgs, false);
        assert!(
            out.contains(r#"handle "weird" input"#),
            "quoted content survives round-trip:\n{out}"
        );
    }

    #[test]
    fn todo_command_applies_incremental_updates_after_the_plan() {
        // Merge regression: `/todo` folds via `reduce_todos`, so a `{action:update}` after the
        // plan is reflected — not just the initial (pending) plan.
        let msgs = vec![tool_call_msg(vec![
            ToolCall {
                id: "1".into(),
                name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"do x","status":"pending"}]}"#.into(),
            },
            ToolCall {
                id: "2".into(),
                name: "todowrite".into(),
                arguments: r#"{"action":"update","id":1,"status":"completed"}"#.into(),
            },
        ])];
        let out = format_todo_command(&msgs, false);
        assert!(out.contains("do x"), "task shown: {out}");
        assert!(
            !out.contains("[ ] do x"),
            "must reflect the completed update, not the pending plan: {out}"
        );
    }

    #[test]
    fn todo_command_ignores_failed_mutations() {
        use atomcode_kernel::message::Message;

        let mut msgs = vec![tool_call_msg(vec![ToolCall {
            id: "ok".into(),
            name: "todowrite".into(),
            arguments:
                r#"{"todos":[{"content":"keep","status":"in_progress"}]}"#.into(),
        }])];
        msgs.push(Message::tool_result("ok", "1 task", false));
        msgs.push(tool_call_msg(vec![ToolCall {
            id: "failed".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"add","content":"must not appear"}"#.into(),
        }]));
        msgs.push(Message::tool_result("failed", "invalid todo", true));

        let out = format_todo_command(&msgs, false);
        assert!(out.contains("keep"), "successful state remains: {out}");
        assert!(
            !out.contains("must not appear"),
            "failed mutation must not enter current state: {out}"
        );
    }

    #[test]
    fn dispatch_required_nonempty_arg_submits() {
        // Sanity counterpart: Required + a real argument must submit the
        // rendered template, never Reject. Guards against an overly-broad
        // rejection rule (e.g. someone drops the `arg.trim().is_empty()`
        // half of the condition).
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Required,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });
        let decision = decide_custom_command(&custom, "myreview", "foo");
        match decision {
            CustomDispatch::Submit(rendered) => assert_eq!(rendered, "review foo"),
            other => panic!("Required + nonempty arg should Submit, got {:?}", other),
        }
    }

    #[test]
    fn dispatch_optional_and_none_empty_arg_submits() {
        // Optional / None commands with empty arg must still submit (the
        // reject rule is scoped to Required only). Locks the boundary so
        // a regression that rejects all empty-arg dispatches is caught.
        for requirement in [ArgsRequirement::Optional, ArgsRequirement::None] {
            let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
            custom.register(crate::custom_commands::CustomCommand {
                name: "myreview".into(),
                description: "".into(),
                args_requirement: requirement.clone(),
                template: "review $ARGUMENTS".into(),
                source: PathBuf::from("x"),
                namespace: None,
            });
            let decision = decide_custom_command(&custom, "myreview", "");
            match decision {
                CustomDispatch::Submit(rendered) => assert_eq!(
                    rendered, "review ",
                    "Optional/None + empty arg should submit rendered template"
                ),
                other => panic!(
                    "{:?} command with empty arg should Submit, got {:?}",
                    requirement, other
                ),
            }
        }
    }

    #[test]
    fn dispatch_unknown_command_is_not_found() {
        // A name that matches neither a custom command nor (in the pure
        // decision layer) a skill falls through to NotFound, which the
        // dispatcher translates into the unknown-command telemetry arm.
        let custom = crate::custom_commands::CustomCommandRegistry::empty();
        let decision = decide_custom_command(&custom, "does_not_exist", "");
        assert!(
            matches!(decision, CustomDispatch::NotFound),
            "Unregistered command should fall through to NotFound, got {:?}",
            decision
        );
    }

    // ── end-to-end: Reject ⇒ render CmdCustomArgRequired, no submit ──────
    //
    // The `decide_custom_command` tests above only pin the pure decision.
    // This block exercises the actual renderer side effect of the `other`
    // arm's `Reject` branch — i.e. that `CmdCustomArgRequired` is rendered
    // as an `UiLine::Error` and, crucially, that `submit_agent_turn` is
    // never reached on this path. If a future refactor drops the
    // `CmdCustomArgRequired` render or flips the arm to silently submit an
    // empty template, these tests catch it.

    /// Minimal `Renderer` mock that records every rendered `UiLine`.
    /// Re-declared here (the `live_snapshot_replay_skips_synthetic_user_messages`
    /// test owns its own copy) so the reject-render tests below stay
    /// self-contained.
    #[derive(Default)]
    struct RecRenderer {
        lines: Vec<UiLine>,
        flushed: usize,
    }
    impl Renderer for RecRenderer {
        fn render(&mut self, line: UiLine) {
            self.lines.push(line);
        }
        fn flush(&mut self) {
            self.flushed += 1;
        }
        fn shutdown(&mut self) {}
        fn reset(&mut self) {}
        fn clear_screen(&mut self) {}
        fn suspend_for_external(&mut self) {}
        fn resume_from_external(&mut self) {}
        fn flush_deferred(&mut self) {}
    }

    #[test]
    fn reject_renders_cmd_custom_arg_required_error() {
        // Trigger: user typed `/myreview` (Required) and pressed Enter
        // with no argument. Expected: exactly one UiLine::Error carrying
        // the CmdCustomArgRequired message, followed by a flush.
        let mut rec = RecRenderer::default();
        render_custom_command_error(&mut rec, &CustomDispatch::Reject, "myreview");

        assert_eq!(
            rec.lines.len(),
            1,
            "Reject must render exactly one line, got {:?}",
            rec.lines
        );
        let rendered = t(Msg::CmdCustomArgRequired { name: "myreview" }).into_owned();
        match &rec.lines[0] {
            UiLine::Error(msg) => assert_eq!(
                msg, &rendered,
                "Reject must render the CmdCustomArgRequired i18n message"
            ),
            other => panic!("Reject must render UiLine::Error, got {:?}", other),
        }
        assert_eq!(
            rec.flushed, 1,
            "Reject must flush the renderer exactly once"
        );
    }

    #[test]
    fn submit_renders_no_error_line_and_not_found_renders_unknown_command() {
        // Counterpart contract:
        //   - Submit is the only non-error outcome, so it must render NO
        //     error line (the caller forwards the rendered template to
        //     submit_agent_turn instead).
        //   - NotFound IS an error outcome (no custom command, no skill),
        //     so it must render exactly one UiLine::Error carrying the
        //     CmdUnknownCommand i18n message, then flush.
        // Guards against a regression that collapses NotFound into the
        // Submit no-op arm (which would silently swallow unknown commands
        // with zero user-visible feedback).
        let mut rec = RecRenderer::default();
        render_custom_command_error(
            &mut rec,
            &CustomDispatch::Submit("review foo".into()),
            "myreview",
        );
        assert!(
            rec.lines.is_empty(),
            "Submit must not render any error line, got {:?}",
            rec.lines
        );

        let mut rec = RecRenderer::default();
        render_custom_command_error(&mut rec, &CustomDispatch::NotFound, "foo");
        assert_eq!(
            rec.lines.len(),
            1,
            "NotFound must render exactly one line, got {:?}",
            rec.lines
        );
        let expected = t(Msg::CmdUnknownCommand { name: "foo" }).into_owned();
        match &rec.lines[0] {
            UiLine::Error(msg) => assert_eq!(
                msg, &expected,
                "NotFound must render the CmdUnknownCommand i18n message"
            ),
            other => panic!("NotFound must render UiLine::Error, got {:?}", other),
        }
        assert_eq!(
            rec.flushed, 1,
            "NotFound must flush the renderer exactly once"
        );
    }

    #[test]
    fn required_empty_arg_dispatch_pipeline_rejects_without_submit() {
        // End-to-end guard for the `other` arm of execute_slash_command_impl.
        // Compose the two pure functions the arm calls — decide_custom_command
        // then render_custom_command_error — and assert the observable
        // contract:
        //   1. Reject ⇒ render exactly one Error line with CmdCustomArgRequired
        //      (the dispatcher never reaches submit_agent_turn on this branch).
        //   2. Submit ⇒ render no error line (submit happens elsewhere, but
        //      we assert the error-render half stays silent on success).
        // This is the closest unit-testable seam to the real dispatcher; the
        // full LoopCtx is intentionally avoided (it'd require constructing an
        // AgentClient + Telemetry + a dozen channels just to reach one arm).
        let mut custom = crate::custom_commands::CustomCommandRegistry::empty();
        custom.register(crate::custom_commands::CustomCommand {
            name: "myreview".into(),
            description: "".into(),
            args_requirement: ArgsRequirement::Required,
            template: "review $ARGUMENTS".into(),
            source: PathBuf::from("x"),
            namespace: None,
        });

        // Reject case: Required + empty arg.
        let mut rec = RecRenderer::default();
        let decision = decide_custom_command(&custom, "myreview", "");
        assert!(
            matches!(decision, CustomDispatch::Reject),
            "Required + empty arg must decide Reject, got {:?}",
            decision
        );
        render_custom_command_error(&mut rec, &decision, "myreview");
        // Contract: exactly one Error line carrying the i18n message.
        let expected = t(Msg::CmdCustomArgRequired { name: "myreview" }).into_owned();
        let errors: Vec<&String> = rec
            .lines
            .iter()
            .filter_map(|line| match line {
                UiLine::Error(msg) => Some(msg),
                _ => None,
            })
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "Reject must render exactly one Error line, got {:?}",
            rec.lines
        );
        assert_eq!(
            errors[0], &expected,
            "Reject must render the CmdCustomArgRequired message"
        );
        // The reject branch never renders anything other than the Error line,
        // which (combined with the decide_custom_command assertion above) means
        // submit_agent_turn is structurally unreachable on this path.
        assert_eq!(
            rec.lines.len(),
            1,
            "Reject must render exactly one line total, got {:?}",
            rec.lines
        );

        // Submit case: Required + nonempty arg must NOT render an error.
        let mut rec = RecRenderer::default();
        let decision = decide_custom_command(&custom, "myreview", "foo");
        match decision {
            CustomDispatch::Submit(ref rendered) => assert_eq!(rendered, "review foo"),
            other => panic!("Required + nonempty arg should Submit, got {:?}", other),
        }
        render_custom_command_error(&mut rec, &decision, "myreview");
        assert!(
            rec.lines
                .iter()
                .all(|line| !matches!(line, UiLine::Error(_))),
            "Submit must not render any Error line, got {:?}",
            rec.lines
        );
    }

    #[test]
    fn init_submits_the_coding_init_prompt() {
        // handler 用 atomcode_coding::INIT_PROMPT 作为提交文本;这里锁定接线源。
        assert!(atomcode_coding::INIT_PROMPT.contains("AGENTS.md"));
    }

    #[test]
    fn cost_report_keeps_models_separate_and_hides_unknown_price() {
        use crate::event_loop::commands::build_cost_report_text;
        use atomcode_capabilities::session::{ModelCostSummary, SessionCostReport, TokenBreakdown};
        let out = build_cost_report_text(
            SessionCostReport {
                models: vec![ModelCostSummary {
                    provider_id: "provider-a".into(),
                    model_id: "model-a".into(),
                    tokens: TokenBreakdown {
                        input: 1234,
                        output: 567,
                        cached_input: 89,
                    },
                    estimated_cost_usd: None,
                    explicitly_free: false,
                }],
                unattributed_tokens: 0,
                total_tokens: 1890,
                estimated_cost_usd: None,
            },
            &atomcode_config::config::Config::default(),
            "provider-b",
            "model-b",
            None,
        );
        // Unknown ids (not in the catalog) fall back to the raw id; separator is `·`.
        assert!(out.contains("provider-a · model-a"));
        assert!(out.contains("provider-b · model-b"));
        assert!(out.contains("1323"));
        assert!(out.contains("567"));
        assert!(out.contains("89"));
        assert!(!out.contains("Estimated cost"));
        assert!(!out.contains("unknown"));
    }
}

#[cfg(test)]
mod mcp_subcommand_tests {
    use super::{parse_mcp_subcommand, McpSub};

    #[test]
    fn mcp_trust_subcommands_recognized() {
        assert!(matches!(parse_mcp_subcommand("trust"), Some(McpSub::Trust)));
        assert!(matches!(
            parse_mcp_subcommand("untrust"),
            Some(McpSub::Untrust)
        ));
    }

    #[test]
    fn mcp_trust_case_insensitive() {
        assert!(matches!(parse_mcp_subcommand("TRUST"), Some(McpSub::Trust)));
        assert!(matches!(
            parse_mcp_subcommand("UnTrust"),
            Some(McpSub::Untrust)
        ));
    }

    #[test]
    fn mcp_existing_subcommands_still_recognized() {
        assert!(matches!(
            parse_mcp_subcommand("reload"),
            Some(McpSub::Reload)
        ));
        assert!(matches!(
            parse_mcp_subcommand("tools myserver"),
            Some(McpSub::Tools)
        ));
        assert!(matches!(
            parse_mcp_subcommand("login github"),
            Some(McpSub::Login)
        ));
        assert!(matches!(
            parse_mcp_subcommand("logout github"),
            Some(McpSub::Logout)
        ));
    }

    #[test]
    fn mcp_unknown_subcommand_returns_none() {
        assert!(parse_mcp_subcommand("").is_none());
        assert!(parse_mcp_subcommand("status").is_none());
        assert!(parse_mcp_subcommand("foobar").is_none());
    }
}

#[cfg(test)]
mod split_skill_names_tests {
    use super::split_skill_names;

    /// 测试用假解析器：返回 skill 的规范身份（小写）。镜像真实 `SkillRegistry::get`
    /// 的大小写不敏感解析——`resolve("BrainStorming")` 与 `resolve("brainstorming")`
    /// 返回同一规范名，所以去重按规范身份而非原始拼写。
    fn resolve(name: &str) -> Option<String> {
        let canonical = name.to_ascii_lowercase();
        matches!(
            canonical.as_str(),
            "adapt-agent" | "skill-creator" | "brainstorming" | "a"
        )
        .then_some(canonical)
    }

    #[test]
    fn multiple_skills_then_task() {
        let (skills, task) = split_skill_names("adapt-agent skill-creator 路径在哪", resolve);
        assert_eq!(skills, vec!["adapt-agent", "skill-creator"]);
        assert_eq!(task, "路径在哪");
    }

    #[test]
    fn single_skill_with_task_unchanged() {
        let (skills, task) = split_skill_names("brainstorming 做个登录页", resolve);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "做个登录页");
    }

    #[test]
    fn single_skill_no_task_unchanged() {
        let (skills, task) = split_skill_names("brainstorming", resolve);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "");
    }

    #[test]
    fn first_token_not_a_skill_yields_empty() {
        let (skills, task) = split_skill_names("路径在哪", resolve);
        assert!(skills.is_empty());
        assert_eq!(task, "路径在哪");
    }

    #[test]
    fn typo_second_skill_falls_into_task() {
        let (skills, task) = split_skill_names("adapt-agent skil-creator 路径在哪", resolve);
        assert_eq!(skills, vec!["adapt-agent"]);
        assert_eq!(task, "skil-creator 路径在哪");
    }

    #[test]
    fn duplicate_skill_deduped() {
        let (skills, task) = split_skill_names("a a 任务", resolve);
        assert_eq!(skills, vec!["a"]);
        assert_eq!(task, "任务");
    }

    #[test]
    fn task_whitespace_preserved_verbatim() {
        let (skills, task) = split_skill_names("brainstorming line1\n  line2", resolve);
        assert_eq!(skills, vec!["brainstorming"]);
        assert_eq!(task, "line1\n  line2");
    }

    #[test]
    fn variant_spellings_of_same_skill_dedup_to_one() {
        // Different case both resolve to canonical "brainstorming" → one skill,
        // NOT two (which would inject the same skill body twice). The first
        // spelling the user typed is kept for display.
        let (skills, task) = split_skill_names("BrainStorming brainstorming 任务", resolve);
        assert_eq!(skills, vec!["BrainStorming"]);
        assert_eq!(task, "任务");
    }
}
