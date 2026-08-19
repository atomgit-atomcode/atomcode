//! daemon `/live` transport and the shared `/chat` turn-construction helpers.

// This module runs IN the TUI process under `/webui`, so any write to the real
// stdout/stderr corrupts the terminal — diagnostics MUST use the file-sink
// `ctrace!`. These denies catch the common console-print forms when clippy runs;
// the `no_console_prints_in_live_path` test is the always-on backstop (clippy is
// not currently wired into CI). Inert (not an error) under a plain `cargo build`.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::dbg_macro)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex as StdMutex};

use atomcode_capabilities::mcp::McpRegistry;
use atomcode_capabilities::tools::PermissionDecision;
use atomcode_coding::runtime::{CodingRuntimeEvent, CompactionCompletion};
use atomcode_config::config::Config;
use atomcode_kernel::message::{ImageContent, Message as KernelMessage, SessionSnapshot};
use atomcode_telemetry::Telemetry;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use std::sync::OnceLock;

pub(crate) use crate::approval_mode::ApprovalMode;

pub(crate) fn fallback_approval_decision(mode: ApprovalMode) -> PermissionDecision {
    match mode {
        // AcceptEdits auto-approval is implemented by WriteApprovalGate. Any
        // request that still reaches the driver requires a real approver
        // (for example bash or a sensitive path), so missing responders must
        // fail closed.
        ApprovalMode::AcceptEdits | ApprovalMode::Plan => PermissionDecision::Deny,
        ApprovalMode::Build | ApprovalMode::Auto => PermissionDecision::AllowOnce,
    }
}

/// Web/TUI 共同显示并下发给 Coding Runtime 的审批模式。
static LIVE_APPROVAL_MODE: StdMutex<ApprovalMode> = StdMutex::new(ApprovalMode::Build);

#[derive(Serialize)]
pub(crate) struct LiveGoalSnapshot {
    active: bool,
    round: u32,
    elapsed_secs: u64,
    condition: String,
    terminal: Option<String>,
    phase: String,
    last_reason: Option<String>,
}

/// 读取当前生效的审批模式。`pub(crate)` 以便 `/chat` 路径（非 sync webui）也据此
/// 选择 PermissionDecider——否则模式 pill 只在 sync 模式生效。
pub(crate) fn live_current_approval_mode() -> ApprovalMode {
    *LIVE_APPROVAL_MODE.lock().unwrap_or_else(|e| e.into_inner())
}

fn native_runtime_mode(mode: ApprovalMode) -> atomcode_coding::RuntimeMode {
    match mode {
        ApprovalMode::Plan => atomcode_coding::RuntimeMode::Plan,
        ApprovalMode::Auto => atomcode_coding::RuntimeMode::Auto,
        ApprovalMode::AcceptEdits => atomcode_coding::RuntimeMode::AcceptEdits,
        ApprovalMode::Build => atomcode_coding::RuntimeMode::Build,
    }
}

/// 当前审批模式的线格字符串（"build" / "accept_edits" / "bypass" / "plan"），
/// 供 Snapshot / 广播使用。
fn live_current_mode_wire() -> String {
    live_current_approval_mode().wire().to_string()
}

/// Coding Runtime 是工作目录的唯一运行时所有者；未绑定时使用 daemon 项目状态。
fn live_current_working_dir(fallback: &Path) -> std::path::PathBuf {
    crate::native_live::binding()
        .map(|binding| binding.working_dir)
        .unwrap_or_else(|_| fallback.to_path_buf())
}

struct AuthoritativeTerminal {
    snapshot: SessionSnapshot,
}

/// 设置 live 视图审批模式；已绑定 runtime 的调用方另行下发 `SetMode`。
pub fn live_set_mode(mode: ApprovalMode) {
    *LIVE_APPROVAL_MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    if let Ok(binding) = crate::native_live::binding() {
        let _ = crate::native_live::publish_unsequenced(
            &binding,
            atomcode_coding::CodingRuntimeEvent::ModeChanged {
                mode: native_runtime_mode(mode),
            },
        );
    }
}

#[cfg(test)]
pub(crate) struct ScopedApprovalModeForTest {
    _guard: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl ScopedApprovalModeForTest {
    pub(crate) fn new() -> Self {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        live_set_mode(ApprovalMode::Build);
        Self { _guard: guard }
    }
}

#[cfg(test)]
impl Drop for ScopedApprovalModeForTest {
    fn drop(&mut self) {
        live_set_mode(ApprovalMode::Build);
    }
}

/// 同步 daemon 的项目视图状态；运行时切换由 CodingRuntime 的可等待接口负责。
pub fn live_set_working_dir(dir: std::path::PathBuf) {
    let dir = crate::normalize_working_dir_case(dir);

    if let Some(store) = crate::DAEMON_PROJECT.lock().unwrap().as_ref() {
        let store = store.clone();
        let dir = dir.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut project = store.write().await;
                let old_dir = project.working_dir.clone();
                if old_dir != dir {
                    project.previous_dir = Some(old_dir);
                    project.working_dir = dir.clone();
                    project.name = dir
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| "project".to_string());
                    let new_key = atomcode_capabilities::pathnorm::path_case_key(&dir);
                    project
                        .recent_dirs
                        .retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != new_key);
                    project.recent_dirs.insert(0, dir.clone());
                    project.recent_dirs.truncate(5);
                }
            });
        } else {
            let mut project = store.blocking_write();
            let old_dir = project.working_dir.clone();
            if old_dir != dir {
                project.previous_dir = Some(old_dir);
                project.working_dir = dir.clone();
                project.name = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
                let new_key = atomcode_capabilities::pathnorm::path_case_key(&dir);
                project
                    .recent_dirs
                    .retain(|d| atomcode_capabilities::pathnorm::path_case_key(d) != new_key);
                project.recent_dirs.insert(0, dir.clone());
                project.recent_dirs.truncate(5);
            }
        }
    }
}

/// 请求当前 Coding Runtime 恢复指定会话。
pub async fn live_switch_session(
    session_id: String,
) -> Result<atomcode_coding::SessionChanged, crate::live_hub::HubError> {
    crate::native_live::resume_session(session_id).await
}

/// 当前生效的 provider 名由绑定 runtime 投影；未绑定时才回退共享启动默认。
fn live_current_provider() -> String {
    if let Ok(binding) = crate::native_live::binding() {
        return binding.provider;
    }
    Config::load(&Config::default_path())
        .map(|c| c.default_provider)
        .unwrap_or_default()
}

/// Resolve the effective provider key: an explicit `provider_name` override wins,
/// otherwise the config's `default_provider`. Shared by the `/compact` and
/// `/context` commands so both select the same model for the same input.
pub(crate) fn resolve_provider_name(config: &Config, provider_name: Option<&str>) -> String {
    provider_name
        .map(|s| s.to_string())
        // Prefer the canonical selection (`default_model` then legacy
        // `default_provider`) so a new-schema default resolves correctly.
        .or_else(|| config.effective_model_selection())
        .unwrap_or_default()
}

// ============================================================================
/// Split a kernel user message into (text, images). Non-user messages yield empty.
fn extract_user_input(m: &KernelMessage) -> (String, Vec<ImageContent>) {
    use atomcode_kernel::message::Role;
    if m.role == Role::User {
        (m.text.clone(), m.images.clone())
    } else {
        (String::new(), Vec::new())
    }
}

/// Re-attach the VL-stripped originals onto the terminal snapshot. The runtime
/// strips image bytes from the caption it sends a text-only model, so the
/// authoritative terminal messages come back image-less; match each real user
/// turn (in order, skipping synthetics/system) to its `turn_base` twin and copy
/// the original text+images back so the persisted/display conversation keeps the
/// thumbnail. Operates directly on kernel messages (image bytes live in
/// `Message::images`, not a MultiPart wrapper).
fn restore_images_from_turn_base(
    mut messages: Vec<KernelMessage>,
    turn_base: &[KernelMessage],
) -> Vec<KernelMessage> {
    use atomcode_kernel::message::Role;

    let final_user_indexes: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| (msg.role == Role::User && !msg.synthetic).then_some(idx))
        .collect();
    let mut final_user_indexes = final_user_indexes.into_iter();

    for original in turn_base
        .iter()
        .filter(|msg| msg.role == Role::User && !msg.synthetic)
    {
        let Some(idx) = final_user_indexes.next() else {
            continue;
        };
        if original.images.is_empty() {
            continue;
        }

        let Some(final_message) = messages.get_mut(idx) else {
            continue;
        };
        // Only restore when the terminal message lost its images (the common
        // VL-strip case); if it already carries images leave it untouched.
        if !final_message.images.is_empty() {
            continue;
        }
        if !original.text.is_empty() {
            final_message.text = original.text.clone();
        }
        final_message.images = original.images.clone();
    }

    messages
}

fn install_authoritative_terminal_snapshot(
    buffer: &mut Vec<KernelMessage>,
    mut snapshot: SessionSnapshot,
    turn_base: &[KernelMessage],
) {
    snapshot.messages = restore_images_from_turn_base(snapshot.messages, turn_base);
    *buffer = snapshot.messages;
}
fn committed_compaction_snapshot(
    event: &CodingRuntimeEvent,
) -> Result<Option<SessionSnapshot>, &'static str> {
    let CodingRuntimeEvent::CompactionFinished {
        completion: CompactionCompletion::Completed(outcome),
    } = event
    else {
        return Ok(None);
    };
    if !outcome.committed || !outcome.is_manual() {
        return Ok(None);
    }
    let snapshot = outcome
        .committed_snapshot
        .as_deref()
        .ok_or("compact completed without a resumable session snapshot")?;
    Ok(Some(snapshot.clone()))
}

/// Derive the native runtime config for a `/chat` request.
pub(crate) fn chat_runtime_config(
    config: &Config,
    provider_name: &str,
    working_dir: &Path,
    telemetry: Arc<Telemetry>,
) -> atomcode_coding::CodingRuntimeConfig {
    // Resolve through the boundary so a new-schema / folded-CodingPlan selection
    // (which no longer lives in `config.providers`) still builds a runtime.
    let resolved = config.provider_config_for_selection(provider_name);
    let p = resolved.as_ref();
    atomcode_coding::CodingRuntimeConfig {
        api_key: p.and_then(|p| p.api_key.clone()).unwrap_or_default(),
        base_url: p.and_then(|p| p.base_url.clone()).unwrap_or_default(),
        model: p.map(|p| p.model.clone()).unwrap_or_default(),
        supports_vision: p.is_some_and(|p| p.accepts_images()),
        preferred_language: Some(atomcode_config::i18n::resolve_initial_locale(
            None,
            config.language,
        )),
        todo: config.tools.todo.clone(),
        provider_name: provider_name.to_string(),
        working_dir: working_dir.to_path_buf(),
        context_window: p.map(|p| p.context_window as u32).unwrap_or(128_000),
        max_tokens: p.and_then(|p| p.max_tokens).map(|m| m as u32),
        mcp: true,
        telemetry: Some(telemetry),
        datalog: config.datalog.clone(),
        reasoning_history: p.and_then(|p| p.reasoning_history.clone()),
        reasoning_effort: p.and_then(|p| p.reasoning_effort.clone()),
        provider_type: p
            .map(|p| p.provider_type.clone())
            .unwrap_or_else(|| "openai".into()),
        thinking_enabled: p.and_then(|p| p.thinking_enabled),
        thinking_type: p.and_then(|p| p.thinking_type.clone()),
        thinking_keep: p.and_then(|p| p.thinking_keep.clone()),
        // The daemon answers `/chat` approvals at its own seam when an interactive
        // responder is registered; otherwise run_chat_turn_v2 applies the mode-specific,
        // fail-closed fallback. Keep the runtime round-trip enabled here.
        dangerously_skip_permissions: false,
        // Keep the fail-closed approval timeout for the daemon (current behavior).
        interactive: false,
        keep_interrupted_context: config.keep_interrupted_context,
        credential_shell_policy: atomcode_coding::config::credential_shell_policy_from_config(
            config.coding.shell_guard_policy,
        ),
        user_agent: p.and_then(|p| p.user_agent.clone()),
        skip_tls_verify: p.map(|p| p.skip_tls_verify).unwrap_or(false),
        retry_max_attempts: p.and_then(|p| p.retry_max_attempts),
        loop_max_rounds: atomcode_coding::resolve_loop_max_rounds(
            config.loop_config.max_rounds,
            std::env::var("ATOMCODE_LOOP_MAX_ROUNDS").ok().as_deref(),
        ),
        // Turn-level round cap. Reuse the canonical resolver (env > TOML) instead
        // of re-implementing the parse — same pattern as loop_max_rounds above.
        turn_max_rounds: atomcode_coding::resolve_turn_max_rounds(
            config.coding.max_rounds,
            std::env::var("ATOMCODE_TURN_MAX_ROUNDS").ok().as_deref(),
        ),
        subagent_config: Some(Arc::new(config.clone())),
        // Daemon path has no TUI checkpoint picker; keep the hard round-cap.
        round_cap_checkpoint: false,
        // WebUI/daemon projection is intentionally deferred; avoid a hidden
        // auxiliary model request until that driver renders the suggestion.
        next_prompt_suggestions: false,
        lsp: atomcode_coding::config::lsp_settings_from_config(&config.lsp),
    }
}

/// Derive the runtime config for `/live`, whose UI has a complete request/respond
/// transport. Interactive requests park until an answer, cancellation, or shutdown.
pub(crate) fn live_runtime_config(
    config: &Config,
    provider_name: &str,
    working_dir: &Path,
    telemetry: Arc<Telemetry>,
) -> atomcode_coding::CodingRuntimeConfig {
    let mut runtime = chat_runtime_config(config, provider_name, working_dir, telemetry);
    runtime.interactive = true;
    runtime
}

fn send_chat_runtime_error(
    events: &mpsc::UnboundedSender<CodingRuntimeEvent>,
    message: impl Into<String>,
) {
    let _ = events.send(CodingRuntimeEvent::Agent(
        atomcode_kernel::event::AgentEvent::Error {
            message: message.into(),
            http_status: None,
            code: None,
            retryable: None,
        },
    ));
}

async fn await_chat_user_input_response(
    rx: tokio::sync::oneshot::Receiver<serde_json::Value>,
    request_timeout: Option<std::time::Duration>,
) -> serde_json::Value {
    match request_timeout {
        Some(timeout) => tokio::time::timeout(timeout, rx)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(serde_json::Value::Null),
        None => rx.await.unwrap_or(serde_json::Value::Null),
    }
}

/// Drive a native runtime over `conv` and forward its native events to the shared
/// `/chat` consumer. `perm_rx` carries interactive approval decisions from `/chat/permission`
/// (`None` = apply [`fallback_approval_decision`] for the selected mode). The kernel
/// snapshot is written back to `conv` so the caller persists the completed turn.
pub(crate) async fn run_chat_turn_v2(
    session_id: String,
    conv: Arc<Mutex<Vec<KernelMessage>>>,
    runtime_event_tx: mpsc::UnboundedSender<CodingRuntimeEvent>,
    cancel: CancellationToken,
    runtime_cfg: atomcode_coding::CodingRuntimeConfig,
    mut perm_rx: Option<mpsc::UnboundedReceiver<PermissionDecision>>,
    user_input_responders: Option<crate::permission_bridge::UserInputResponders>,
    approval_mode: ApprovalMode,
) {
    use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
    use atomcode_coding::{CodingRuntime, TurnCompletion};

    // Split the just-submitted user input from the persisted prefix before runtime
    // startup. The buffer already holds kernel messages (cold summaries inline as
    // synthetic messages), so the prefix IS a `SessionSnapshot` of the remaining
    // messages — no core round-trip. The prefix is imported/initialized under the
    // target session's lease.
    let (prefix, user_text, user_images, turn_base) = {
        let c = conv.lock().await;
        let turn_base = c.clone();
        let mut msgs = c.clone();
        let last = msgs.pop();
        let (text, images) = last.as_ref().map(extract_user_input).unwrap_or_default();
        (SessionSnapshot::new(msgs), text, images, turn_base)
    };
    // Stash the ORIGINAL image to the display-only sidecar BEFORE it is stripped from the
    // model conversation below (`user_images = Vec::new()`), so a reloading client refills
    // the thumbnail from the sidecar. The /chat path previously skipped this, so the image
    // was lost after refresh for anyone loading the session fresh from disk. Mirrors /live.
    stash_vl_display_images(
        &runtime_cfg.working_dir,
        &session_id,
        &user_text,
        &user_images,
    );
    let naming_session_id = session_id.clone();
    let naming_project_bucket =
        atomcode_capabilities::session::SessionManager::project_hash(&runtime_cfg.working_dir);
    let (runtime, coding_cfg) = match crate::start_native_runtime_with_session(
        runtime_cfg,
        atomcode_coding::SessionMode::ExternalSnapshot {
            id: session_id.clone(),
            snapshot: prefix,
        },
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            send_chat_runtime_error(&runtime_event_tx, error.to_string());
            return;
        }
    };
    let CodingRuntime {
        handle,
        mut events,
        task,
        ..
    } = runtime;
    // VL 预处理后的文本已包含图片描述，原图不再发给 kernel
    // （非视觉模型的 provider adapter 会因原图而报 400 错误）
    let user_images = if text_carries_vl_caption(&user_text) {
        Vec::new()
    } else {
        user_images
    };
    if let Err(error) = handle.set_mode(native_runtime_mode(approval_mode)).await {
        send_chat_runtime_error(&runtime_event_tx, format!("切换模式失败：{error}"));
        return;
    }
    let input = atomcode_coding::UserInput {
        text: user_text,
        images: user_images,
    };
    if let Err(error) = handle.submit(input).await {
        send_chat_runtime_error(&runtime_event_tx, format!("发送用户消息失败：{error}"));
        return;
    }

    let mut cancelled = false;
    let final_messages = loop {
        let ev = tokio::select! {
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = handle.cancel().await;
                continue;
            }
            ev = events.recv() => ev,
        };
        let Some(ev) = ev.map(|event| event.event) else {
            send_chat_runtime_error(
                &runtime_event_tx,
                "coding runtime event stream closed before turn terminal",
            );
            break None;
        };
        match ev {
            event @ CodingRuntimeEvent::Agent(_) => {
                let _ = runtime_event_tx.send(event);
            }
            CodingRuntimeEvent::Request(request) if request.kind == APPROVAL_KIND => {
                let _ = runtime_event_tx.send(CodingRuntimeEvent::Request(request.clone()));
                if serde_json::from_value::<ApprovalRequest>(request.payload).is_err() {
                    let _ = handle.respond(request.id, serde_json::Value::Null).await;
                    continue;
                }
                let decision = match &mut perm_rx {
                    None => fallback_approval_decision(approval_mode),
                    Some(rx) => tokio::select! {
                        _ = cancel.cancelled(), if !cancelled => {
                            cancelled = true;
                            let _ = handle.cancel().await;
                            PermissionDecision::Deny
                        }
                        decision = rx.recv() => decision.unwrap_or(PermissionDecision::Deny),
                    },
                };
                let response = match decision {
                    PermissionDecision::AllowOnce => ApprovalResponse::allow(),
                    PermissionDecision::AllowAlways => ApprovalResponse::allow_always(),
                    _ => ApprovalResponse::deny(),
                };
                let value = serde_json::to_value(response).unwrap_or(serde_json::Value::Null);
                let _ = handle.respond(request.id, value).await;
            }
            CodingRuntimeEvent::Request(request)
                if request.kind
                    == atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND =>
            {
                let Some(responders) = &user_input_responders else {
                    let _ = runtime_event_tx.send(CodingRuntimeEvent::Request(request.clone()));
                    let _ = handle.respond(request.id, serde_json::Value::Null).await;
                    continue;
                };
                let (tx, rx) = tokio::sync::oneshot::channel();
                responders.register(session_id.clone(), request.id, tx);
                // Register before publishing the SSE event so a very fast browser answer
                // cannot race the response route and be rejected as stale.
                let _ = runtime_event_tx.send(CodingRuntimeEvent::Request(request.clone()));
                let answer = await_chat_user_input_response(rx, coding_cfg.request_timeout);
                tokio::pin!(answer);
                let value = tokio::select! {
                    _ = cancel.cancelled(), if !cancelled => {
                        cancelled = true;
                        let _ = handle.cancel().await;
                        serde_json::Value::Null
                    }
                    answer = &mut answer => answer,
                };
                responders.unregister(&session_id, request.id);
                let _ = handle.respond(request.id, value).await;
            }
            CodingRuntimeEvent::Request(request) => {
                let _ = runtime_event_tx.send(CodingRuntimeEvent::Request(request.clone()));
                let _ = handle.respond(request.id, serde_json::Value::Null).await;
            }
            CodingRuntimeEvent::TurnFinished(completion @ TurnCompletion::Completed { .. }) => {
                let snapshot = match &completion {
                    TurnCompletion::Completed { snapshot, .. } => snapshot.clone(),
                    TurnCompletion::SnapshotUnavailable { .. } => unreachable!(),
                };
                let _ = runtime_event_tx.send(CodingRuntimeEvent::TurnFinished(completion));
                break Some(AuthoritativeTerminal {
                    snapshot: snapshot.as_ref().clone(),
                });
            }
            event @ CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                ..
            }) => {
                let _ = runtime_event_tx.send(event);
                break None;
            }
            event @ CodingRuntimeEvent::CompactionStarted { .. }
            | event @ CodingRuntimeEvent::CompactionFinished { .. } => {
                let compact_snapshot = match committed_compaction_snapshot(&event) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        send_chat_runtime_error(&runtime_event_tx, error);
                        continue;
                    }
                };
                if let Some(snapshot) = compact_snapshot {
                    // Cold summaries live inline as synthetic messages in the kernel
                    // snapshot, so replacing the buffer wholesale carries them.
                    let mut buffer = conv.lock().await;
                    *buffer = snapshot.messages;
                }
                let _ = runtime_event_tx.send(event);
            }
            CodingRuntimeEvent::SessionNameSuggested { name } => {
                if let Err(error) = crate::legacy_convert::apply_ai_catalog_name_in_project(
                    &naming_project_bucket,
                    &naming_session_id,
                    &name,
                ) {
                    let _ = runtime_event_tx.send(CodingRuntimeEvent::ControllerWarning(format!(
                        "session naming failed: {error}"
                    )));
                }
            }
            CodingRuntimeEvent::RuntimeStopped(_) => {
                send_chat_runtime_error(
                    &runtime_event_tx,
                    "coding runtime stopped before turn terminal",
                );
                break None;
            }
            event => {
                let _ = runtime_event_tx.send(event);
            }
        }
    };
    if let Some(terminal) = final_messages {
        let mut c = conv.lock().await;
        install_authoritative_terminal_snapshot(&mut c, terminal.snapshot, &turn_base);
    }
    let _ = handle.shutdown().await;
    let _ = task.await;
    // Dropping runtime_event_tx here closes the consumer loop, which then shapes
    // the final HTTP events and sends Done.
}

use crate::AppState;
use axum::{
    extract::{Extension, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Json,
    },
};
use futures::stream::StreamExt;
use serde::Serialize;

// ============================================================================
// Wire DTO: LiveWireEvent + to_wire
// ============================================================================

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum LiveWireEvent {
    #[serde(rename = "snapshot")]
    Snapshot {
        messages: Vec<crate::MessageInfo>,
        session_id: String,
        /// 会话名（Session.name）。让 App 端首次扫码连接就能在顶部显示
        /// 已有会话名,不必等 SessionRenamed 事件(切项目场景才有 loadSession 拉名)。
        /// 加载失败或空会话时为空字符串,App 端回退到项目名。
        session_name: String,
        project_hash: String,
        provider: String,
        /// 当前审批模式（build / accept_edits / bypass / plan），
        /// 让新连上的 tab 立刻显示正确的模式 pill。
        mode: String,
        /// 当前工作目录，让 App 端能展示项目名。
        #[serde(rename = "working_dir")]
        working_dir: String,
        /// Goal controller state is not persisted in SessionSnapshot, so the
        /// initial wire snapshot carries it explicitly for late subscribers.
        goal: Option<LiveGoalSnapshot>,
    },
    #[serde(rename = "provider")]
    Provider { provider: String },
    #[serde(rename = "reasoning_effort")]
    ReasoningEffort {
        provider: String,
        effort: Option<String>,
        applicable: bool,
    },
    /// 审批模式切换（build / accept_edits / bypass / plan）——
    /// webui 各 tab 的「模式」pill 据此同步。
    #[serde(rename = "mode")]
    Mode { mode: String },
    /// Goal controller progress. This is deliberately structured so mobile
    /// clients can distinguish completion, failure and user cancellation.
    #[serde(rename = "goal_changed")]
    GoalChanged {
        active: bool,
        round: u32,
        elapsed_secs: u64,
        condition: String,
        terminal: Option<String>,
        phase: String,
        last_reason: Option<String>,
    },
    /// 斜杠命令的文本输出（如 /status 报告）。`text` 首行即 `/cmd` 标头，
    /// 前端整体显示为一条系统消息即可。
    #[serde(rename = "command_output")]
    CommandOutput { text: String },
    #[serde(rename = "user")]
    UserMessage {
        text: String,
        images: Vec<crate::ImageData>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_input_id: Option<String>,
    },
    #[serde(rename = "text")]
    TextDelta { content: String },
    #[serde(rename = "reasoning")]
    ReasoningDelta { content: String },
    #[serde(rename = "tool_start")]
    ToolStart {
        id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "tool_progress")]
    ToolProgress { id: String, progress: String },
    #[serde(rename = "tool_result")]
    ToolResult {
        id: String,
        name: String,
        output: String,
        success: bool,
        duration_ms: u64,
    },
    #[serde(rename = "tokens")]
    Tokens {
        prompt: usize,
        completion: usize,
        total: usize,
    },
    #[serde(rename = "state")]
    State {
        running: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        stats: Option<crate::TurnStatsWire>,
    },
    #[serde(rename = "error")]
    Error { message: String },
    /// Non-fatal advisory (e.g. "conversation compacted"). A distinct severity from
    /// `Error` so a client can render it as a muted notice instead of a red error.
    #[serde(rename = "warning")]
    Warning { message: String },
    /// Auxiliary session persistence failed. Kept distinct from conversational
    /// warnings so browser clients render it outside the message timeline.
    #[serde(rename = "persistence_warning")]
    PersistenceWarning { message: String },
    #[serde(rename = "permission_request")]
    PermissionRequest {
        tool_name: String,
        reason: String,
        call_id: String,
        arguments: String,
    },
    #[serde(rename = "user_input_request")]
    UserInputRequest {
        request_id: u64,
        header: String,
        question: String,
        mode: String,
        options: Vec<serde_json::Value>,
        /// Present for a multi-question batch (each item is a `{header,question,mode,options}`
        /// object). Omitted for a single question — the webui then uses the flat fields above.
        #[serde(skip_serializing_if = "Option::is_none")]
        questions: Option<Vec<serde_json::Value>>,
        /// Whether to offer the "type your own answer" row (single question). Default true.
        custom: bool,
    },
    #[serde(rename = "user_input_resolved")]
    UserInputResolved { request_id: u64 },
    #[serde(rename = "policy_intervention")]
    PolicyIntervention {
        intervention_id: u64,
        code: atomcode_kernel::event::PolicyInterventionCode,
        actions: Vec<atomcode_kernel::event::PolicyRecoveryAction>,
    },
    #[serde(rename = "policy_intervention_resolved")]
    PolicyInterventionResolved {
        intervention_id: u64,
        action: atomcode_kernel::event::PolicyRecoveryAction,
    },
    #[serde(rename = "policy_intervention_cleared")]
    PolicyInterventionCleared { intervention_id: u64 },
    /// One or more live inputs were folded into the active turn. The exact
    /// payload lets browser clients acknowledge their FIFO pending-steer UI
    /// without guessing from text deltas or turn terminals.
    #[serde(rename = "steered")]
    Steered {
        count: usize,
        inputs: Vec<atomcode_kernel::event::SteeredInput>,
        client_input_ids: Vec<Option<String>>,
    },
    #[serde(rename = "session_switched")]
    SessionSwitched { session_id: String },
    /// AI auto-renamed a session (daemon AI namer). Carries `session_id` so a
    /// tab only updates its title when IT is viewing that session — the live
    /// broadcast reaches every subscribed tab, so an unscoped update would flip
    /// the title of tabs viewing other sessions.
    #[serde(rename = "session_renamed")]
    SessionRenamed { session_id: String, name: String },
    /// Working directory switched (any view's `/cd`). Every webui tab updates its
    /// path display + session-list filter to follow. Carries the absolute path.
    #[serde(rename = "working_dir")]
    WorkingDir { working_dir: String },
    /// Rate-limit hit: provider has throttled requests. Carries display-ready reset
    /// time and label so the webui can render a countdown notice instead of a generic error.
    #[serde(rename = "rate_limited")]
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        #[serde(default)]
        auto_resuming: bool,
        /// Provider's own 429 message (no `HTTP …:` prefix), for the generic pause.
        #[serde(default)]
        server_message: Option<String>,
    },
}

#[derive(Default)]
struct NativeLiveWireProjector {
    tools: HashMap<String, (String, std::time::Instant)>,
    session_id: String,
}

impl NativeLiveWireProjector {
    fn project(&mut self, event: crate::live_hub::LiveViewEvent) -> Option<LiveWireEvent> {
        use atomcode_capabilities::tools::{
            request_user_input::REQUEST_USER_INPUT_KIND, ApprovalRequest, APPROVAL_KIND,
        };
        use atomcode_coding::CodingRuntimeEvent as Runtime;
        use atomcode_kernel::event::AgentEvent as Kernel;

        Some(match event {
            crate::live_hub::LiveViewEvent::CommandOutput(text) => {
                LiveWireEvent::CommandOutput { text }
            }
            crate::live_hub::LiveViewEvent::InputAccepted {
                input,
                client_input_id,
            } => LiveWireEvent::UserMessage {
                text: input.text,
                images: input
                    .images
                    .into_iter()
                    .map(|image| crate::ImageData {
                        media_type: image.media_type,
                        data: image.data,
                        missing: false,
                    })
                    .collect(),
                client_input_id,
            },
            crate::live_hub::LiveViewEvent::Steered {
                count,
                inputs,
                client_input_ids,
            } => LiveWireEvent::Steered {
                count,
                inputs,
                client_input_ids,
            },
            crate::live_hub::LiveViewEvent::RequestResolved { request_id, kind } => {
                if kind == atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND
                {
                    LiveWireEvent::UserInputResolved { request_id }
                } else {
                    return None;
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::Agent(event)) => match event {
                Kernel::TurnStarted => LiveWireEvent::State {
                    running: true,
                    stop_reason: None,
                    message: None,
                    stats: None,
                },
                Kernel::TextDelta(content) => LiveWireEvent::TextDelta { content },
                Kernel::Reasoning(content) => LiveWireEvent::ReasoningDelta { content },
                Kernel::ToolStarted { call } => {
                    self.tools.insert(
                        call.id.clone(),
                        (call.name.clone(), std::time::Instant::now()),
                    );
                    LiveWireEvent::ToolStart {
                        id: call.id,
                        name: call.name,
                        arguments: call.arguments,
                    }
                }
                Kernel::ToolProgress { call_id, message } => LiveWireEvent::ToolProgress {
                    id: call_id,
                    progress: message,
                },
                Kernel::ToolResult { result } => {
                    let (name, started) = self
                        .tools
                        .remove(&result.call_id)
                        .unwrap_or_else(|| ("tool".into(), std::time::Instant::now()));
                    LiveWireEvent::ToolResult {
                        id: result.call_id,
                        name,
                        output: result.content,
                        success: !result.is_error,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }
                }
                Kernel::PolicyIntervention { intervention } => LiveWireEvent::PolicyIntervention {
                    intervention_id: intervention.id,
                    code: intervention.code,
                    actions: intervention.actions,
                },
                Kernel::Usage(meta) => LiveWireEvent::Tokens {
                    prompt: meta.tokens.prompt as usize,
                    completion: meta.tokens.completion as usize,
                    total: (meta.tokens.prompt + meta.tokens.completion) as usize,
                },
                Kernel::Error { message, .. } => LiveWireEvent::Error { message },
                Kernel::Warning(message) => LiveWireEvent::Warning { message },
                Kernel::StreamRecovery {
                    attempt,
                    max_attempts,
                    recovered,
                } => LiveWireEvent::Warning {
                    message: if recovered {
                        "recovered from the interrupted stream".to_string()
                    } else {
                        format!(
                            "stream timed out; safely continuing from saved progress ({attempt}/{max_attempts})"
                        )
                    },
                },
                Kernel::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                    server_message,
                } => LiveWireEvent::RateLimited {
                    reset_at_display,
                    reset_label,
                    secs_until_reset,
                    auto_resuming,
                    server_message,
                },
                Kernel::ToolCallStreaming { .. }
                | Kernel::ToolBatchStarted { .. }
                | Kernel::ToolBatchCompleted { .. }
                | Kernel::Request { .. }
                | Kernel::Snapshot { .. }
                | Kernel::TurnComplete { .. }
                | Kernel::Cancelled
                | Kernel::CompactionStarted { .. }
                | Kernel::Compacted { .. }
                | Kernel::CompactionFailed { .. } => return None,
                Kernel::Steered { .. } => return None,
                _ => return None,
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::Request(request)) => {
                if request.kind == APPROVAL_KIND {
                    let approval: ApprovalRequest = serde_json::from_value(request.payload).ok()?;
                    LiveWireEvent::PermissionRequest {
                        tool_name: approval.tool,
                        reason: "Requires approval".into(),
                        call_id: approval.call_id,
                        arguments: approval.args,
                    }
                } else if request.kind == REQUEST_USER_INPUT_KIND {
                    LiveWireEvent::UserInputRequest {
                        request_id: request.id,
                        header: request
                            .payload
                            .get("header")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        question: request
                            .payload
                            .get("question")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        mode: request
                            .payload
                            .get("mode")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("single")
                            .to_string(),
                        options: request
                            .payload
                            .get("options")
                            .and_then(serde_json::Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                        questions: request
                            .payload
                            .get("questions")
                            .and_then(serde_json::Value::as_array)
                            .cloned(),
                        custom: request
                            .payload
                            .get("custom")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(true),
                    }
                } else {
                    return None;
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::TurnFinished(completion)) => {
                self.tools.clear();
                match completion {
                    atomcode_coding::TurnCompletion::Completed { reason, stats, .. } => {
                        LiveWireEvent::State {
                            running: false,
                            stop_reason: Some(crate::stop_reason_wire(reason).to_string()),
                            message: None,
                            stats: Some(stats.into()),
                        }
                    }
                    atomcode_coding::TurnCompletion::SnapshotUnavailable {
                        error, stats, ..
                    } => LiveWireEvent::State {
                        running: false,
                        stop_reason: Some("snapshot_unavailable".into()),
                        message: Some(error.message),
                        stats: Some(stats.into()),
                    },
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ModeChanged { mode }) => {
                LiveWireEvent::Mode {
                    mode: mode.wire().into(),
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::GoalChanged(progress)) => {
                use atomcode_coding::{GoalPhase, GoalTerminal};
                LiveWireEvent::GoalChanged {
                    active: progress.active,
                    round: progress.round,
                    elapsed_secs: progress.elapsed_secs,
                    condition: progress.condition,
                    terminal: progress.terminal.map(|terminal| {
                        match terminal {
                            GoalTerminal::Met => "met",
                            GoalTerminal::Stopped => "stopped",
                            GoalTerminal::Failed => "failed",
                            GoalTerminal::Cancelled => "cancelled",
                        }
                        .into()
                    }),
                    phase: match progress.phase {
                        GoalPhase::Pursuing => "pursuing",
                        GoalPhase::Paused => "paused",
                        GoalPhase::PausedAtCap => "paused_at_cap",
                        GoalPhase::Satisfied => "satisfied",
                        GoalPhase::Ended => "ended",
                    }
                    .into(),
                    last_reason: progress.last_reason,
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ProviderChanged {
                provider, ..
            }) => LiveWireEvent::Provider { provider },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ReasoningEffortChanged {
                provider,
                effort,
                applicable,
            }) => LiveWireEvent::ReasoningEffort {
                provider,
                effort: effort.map(|value| value.as_str().to_string()),
                applicable,
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::SessionNameSuggested { name }) => {
                LiveWireEvent::SessionRenamed {
                    session_id: self.session_id.clone(),
                    name,
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::PolicyInterventionResolved {
                intervention_id,
                action,
            }) => LiveWireEvent::PolicyInterventionResolved {
                intervention_id,
                action,
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::PolicyInterventionCleared {
                intervention_id,
            }) => LiveWireEvent::PolicyInterventionCleared { intervention_id },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::SessionChanged(changed)) => {
                let session_id = changed.session_id?;
                self.session_id = session_id.clone();
                LiveWireEvent::SessionSwitched { session_id }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::WorkingDirectoryChanged(
                working_dir,
            )) => LiveWireEvent::WorkingDir {
                working_dir: working_dir.to_string_lossy().to_string(),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ControllerWarning(message)) => {
                LiveWireEvent::Warning { message }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::PersistenceWarning(message)) => {
                LiveWireEvent::PersistenceWarning { message }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::RuntimeStopped(exit)) => {
                self.tools.clear();
                LiveWireEvent::State {
                    running: false,
                    stop_reason: Some("runtime_stopped".into()),
                    message: Some(format!(
                        "coding runtime stopped: {:?}{}",
                        exit.reason,
                        if exit.forced { " (forced)" } else { "" }
                    )),
                    stats: None,
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(Runtime::CompactionFinished {
                completion: CompactionCompletion::Completed(outcome),
            }) if outcome.committed => LiveWireEvent::Warning {
                message: atomcode_config::i18n::format_compaction_mark(
                    outcome.removed_messages,
                    outcome.estimated_tokens_before,
                    outcome.estimated_tokens_after,
                ),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::CompactionFinished {
                completion: CompactionCompletion::Failed { error, .. },
            }) => LiveWireEvent::Error {
                message: format!("compact failed: {error}"),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ProviderUnavailable {
                reason,
                ..
            }) => LiveWireEvent::Error {
                message: reason.to_string(),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ProviderReloadFinished(Err(
                error,
            ))) => LiveWireEvent::Error {
                message: format!("provider reload failed: {error}"),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::ProviderDeactivationFinished(
                Err(error),
            )) => LiveWireEvent::Error {
                message: format!("provider deactivation failed: {error}"),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::SnapshotRestoreFinished {
                result: Err(error),
                ..
            }) => LiveWireEvent::Error {
                message: format!("snapshot restore failed: {error}"),
            },
            crate::live_hub::LiveViewEvent::Runtime(Runtime::UndoFinished(Err(error))) => {
                LiveWireEvent::Error {
                    message: format!("undo failed: {error}"),
                }
            }
            crate::live_hub::LiveViewEvent::Runtime(_) => return None,
        })
    }
}

fn goal_snapshot(progress: &atomcode_coding::GoalProgress) -> LiveGoalSnapshot {
    use atomcode_coding::{GoalPhase, GoalTerminal};
    LiveGoalSnapshot {
        active: progress.active,
        round: progress.round,
        elapsed_secs: progress.elapsed_secs,
        condition: progress.condition.clone(),
        terminal: progress.terminal.map(|terminal| {
            match terminal {
                GoalTerminal::Met => "met",
                GoalTerminal::Stopped => "stopped",
                GoalTerminal::Failed => "failed",
                GoalTerminal::Cancelled => "cancelled",
            }
            .into()
        }),
        phase: match progress.phase {
            GoalPhase::Pursuing => "pursuing",
            GoalPhase::Paused => "paused",
            GoalPhase::PausedAtCap => "paused_at_cap",
            GoalPhase::Satisfied => "satisfied",
            GoalPhase::Ended => "ended",
        }
        .into(),
        last_reason: progress.last_reason.clone(),
    }
}

// ============================================================================
// Handlers: GET /live (SSE) + POST /live/message
// ============================================================================

/// 规范化前端传来的 session id（None/空字符串 → None）。
/// 仅做解析、不读盘；严格的历史加载由 native runtime 绑定流程负责。
fn parse_session_id(session_id_str: Option<String>) -> Option<String> {
    session_id_str.and_then(|id| {
        let id = id.trim();
        (!id.is_empty()).then(|| id.to_string())
    })
}

fn provider_reload_required(
    active: &str,
    active_fingerprint: &str,
    requested: &str,
    requested_fingerprint: &str,
) -> bool {
    active != requested || active_fingerprint != requested_fingerprint
}

/// GET /live 查询参数。`session_id` 可选：提供时绑定到该 native session。
#[derive(serde::Deserialize, Default)]
pub(crate) struct LiveStreamQuery {
    #[serde(default)]
    pub session_id: Option<String>,
}

fn serialize_scoped_live_event(session_id: String, event: &LiveWireEvent) -> String {
    let mut value = match serde_json::to_value(event) {
        Ok(value) => value,
        Err(error) => {
            crate::ctrace!("LIVE", "live event serialization failed: {error}");
            return serde_json::json!({
                "type": "error",
                "session_id": session_id,
                "message": format!("live event serialization failed: {error}"),
            })
            .to_string();
        }
    };
    if let Some(object) = value.as_object_mut() {
        object.insert("session_id".into(), serde_json::Value::String(session_id));
        return serde_json::to_string(&value).unwrap_or_else(|error| {
            crate::ctrace!("LIVE", "live event JSON serialization failed: {error}");
            serde_json::json!({
                "type": "error",
                "message": format!("live event serialization failed: {error}"),
            })
            .to_string()
        });
    }
    serde_json::json!({
        "type": "error",
        "session_id": session_id,
        "message": "live event is not a JSON object",
    })
    .to_string()
}

#[cfg(test)]
mod scoped_live_event_tests {
    use super::{serialize_scoped_live_event, LiveWireEvent};

    #[test]
    fn every_wire_event_gets_session_id() {
        let json = serialize_scoped_live_event(
            "session-a".into(),
            &LiveWireEvent::TextDelta {
                content: "hello".into(),
            },
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "text");
        assert_eq!(value["session_id"], "session-a");
    }
}

pub(crate) async fn live_stream(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<LiveStreamQuery>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    let sid = parse_session_id(q.session_id);
    let join = match crate::native_live::ensure_headless_runtime(
        live_current_working_dir(&working_dir),
        state.telemetry.clone(),
        live_current_provider(),
        native_runtime_mode(live_current_approval_mode()),
        sid,
    )
    .await
    {
        Ok(join) => join,
        Err(error) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": error })),
            )
                .into_response()
        }
    };
    let snapshot_wd = join.binding.working_dir.clone();
    let project_hash = atomcode_capabilities::session::SessionManager::project_hash(&snapshot_wd);
    let (session_name, session_meta, turn_timestamps) = {
        match crate::legacy_convert::load_catalog_session_view_in_project(
            &project_hash,
            &join.binding.session_id,
        ) {
            Ok(Some(session)) => {
                let timestamps =
                    crate::load_turn_timestamps(&project_hash, &join.binding.session_id).await;
                (session.meta.name.clone(), Some(session.meta), timestamps)
            }
            Ok(None) => (String::new(), None, crate::TurnTimestamps::new()),
            Err(error) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": error.to_string() })),
                )
                    .into_response()
            }
        }
    };
    let initial_goal = join.goal_progress.clone();
    // Keep the session id alongside the wire event until serialization.  The
    // projector can update its session after a SessionChanged event, so the id
    // must be captured at the exact point where each event is projected.
    let (tx, out_rx) = mpsc::unbounded_channel::<(String, LiveWireEvent)>();
    let mut snapshot_messages: Vec<crate::MessageInfo> = join
        .snapshot
        .messages
        .iter()
        .map(crate::MessageInfo::from_kernel)
        .collect();
    if let Some(meta) = session_meta.as_ref() {
        crate::attach_snapshot_message_timestamps(
            &mut snapshot_messages,
            &join.snapshot,
            meta,
            &turn_timestamps,
        );
    }
    // Re-attach display-only images (VL-preprocessed originals) so a refresh — which
    // rebuilds from the kernel snapshot (image stripped) — shows the thumbnail, not the
    // "missing image" placeholder. Same sidecar the HTTP session-load path reads.
    crate::attach_display_images(
        &mut snapshot_messages,
        &snapshot_wd,
        &join.binding.session_id,
    );
    let _ = tx.send((
        join.binding.session_id.clone(),
        LiveWireEvent::Snapshot {
            messages: snapshot_messages,
            session_id: join.binding.session_id.clone(),
            session_name,
            project_hash,
            provider: join.binding.provider.clone(),
            mode: live_current_mode_wire(),
            working_dir: snapshot_wd.to_string_lossy().to_string(),
            goal: initial_goal.as_ref().map(goal_snapshot),
        },
    ));
    let mut projector = NativeLiveWireProjector {
        session_id: join.binding.session_id.clone(),
        ..Default::default()
    };
    // Goal is controller state rather than persisted conversation history. It
    // is therefore restored separately from the message snapshot so an App
    // connecting after the TUI turn has finished still sees the Goal badge.
    if let Some(goal) = join.goal_progress {
        if let Some(w) = projector.project(crate::live_hub::LiveViewEvent::Runtime(
            atomcode_coding::CodingRuntimeEvent::GoalChanged(goal),
        )) {
            let _ = tx.send((projector.session_id.clone(), w));
        }
    }
    for observation in join.replay {
        if let Some(w) = projector.project(observation.event) {
            let _ = tx.send((projector.session_id.clone(), w));
        }
    }
    let binding_id = join.binding.id;
    let mut rx = join.receiver;
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(observation) if observation.binding_id == binding_id => {
                    if let Some(w) = projector.project(observation.event) {
                        let scoped = (projector.session_id.clone(), w);
                        if tx.send(scoped).is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => break,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let _ = tx.send((
                        projector.session_id.clone(),
                        LiveWireEvent::Error {
                            message: format!("live stream lagged by {skipped} events; reconnect"),
                        },
                    ));
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream =
        tokio_stream::wrappers::UnboundedReceiverStream::new(out_rx).map(|(session_id, event)| {
            let json = serialize_scoped_live_event(session_id, &event);
            Ok::<_, std::convert::Infallible>(Event::default().data(json))
        });
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("ping"),
        )
        .into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveMessageReq {
    pub message: String,
    /// Opaque browser-generated correlation id. It is projected back on input
    /// echo and steer acknowledgement, but never enters model context.
    #[serde(default)]
    pub client_input_id: Option<String>,
    #[serde(default)]
    pub images: Vec<crate::ImageInput>,
    /// webui 选中的模型（provider 名）。Some 时切换当前绑定 runtime。
    #[serde(default)]
    pub provider: Option<String>,
    /// 调用方的当前 session_id。
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Apply the shared daemon image-preprocessing policy to one user caption.
///
/// Build the runtime input for a pasted image and report the matching UI notice.
/// Native vision and unconfigured VL paths preserve the original image bytes; successful
/// preprocessing and explicit VL failures return text-only input for text models.
pub(crate) async fn preprocess_image_input(
    config: &Config,
    supports_vision: bool,
    working_dir: &std::path::Path,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<&str>,
    message: &str,
    images: &[ImageContent],
) -> (
    atomcode_coding::UserInput,
    Option<atomcode_coding::VisionNotice>,
) {
    use atomcode_coding::vision::{
        run_vl_caption, should_skip, vl_model_display, PreprocessOutcome,
    };
    // Short-circuit: no images, or the main model already accepts images.
    if should_skip(supports_vision, !images.is_empty()) {
        return (
            atomcode_coding::UserInput {
                text: message.to_string(),
                images: images.to_vec(),
            },
            None,
        );
    }
    // Nothing configured (None or empty) ⇒ pass through unchanged (Skipped).
    let Some(vl_name) = config
        .vision_preprocessor_provider
        .clone()
        .filter(|s| !s.is_empty())
    else {
        return (
            atomcode_coding::UserInput {
                text: message.to_string(),
                images: images.to_vec(),
            },
            None,
        );
    };
    // Configured but absent from `config.providers` ⇒ Failed (mirror the retired
    // core `maybe_preprocess`): fold the failure marker so the caller strips the
    // images — otherwise raw image bytes reach a text-only model (HTTP 400).
    let Some(vl_pc) = config.provider_config_for_selection(&vl_name) else {
        let reason = format!("VL provider '{vl_name}' not found in config");
        return (
            atomcode_coding::UserInput {
                text: fold_vl_failure(message),
                images: Vec::new(),
            },
            Some(atomcode_coding::VisionNotice::Failed { reason }),
        );
    };
    let vl_model = vl_model_display(&vl_pc.model).to_string();
    // Build the one-off VL provider via the daemon's native chain (the SAME
    // chain `/chat` and `/compact` use), yielding a kernel-native provider —
    // no core provider. `build` may block on auth I/O (gateway token) → run it
    // off the async runtime. `session_id` is bound at build so the VL call
    // rides the same upstream account/replica as the main turn.
    let coding_cfg = crate::kernel_runtime::coding_config_from_runtime(&chat_runtime_config(
        config,
        &vl_name,
        working_dir,
        telemetry,
    ));
    let factory = crate::runtime_host::coding_provider_factory();
    let sid = session_id.filter(|s| !s.is_empty()).map(|s| s.to_string());
    let provider =
        match tokio::task::spawn_blocking(move || factory.build(&coding_cfg, sid.as_deref())).await
        {
            Ok(Ok(p)) => p,
            _ => {
                let reason = format!("VL provider '{vl_name}' build failed");
                return (
                    atomcode_coding::UserInput {
                        text: fold_vl_failure(message),
                        images: Vec::new(),
                    },
                    Some(atomcode_coding::VisionNotice::Failed { reason }),
                );
            }
        };
    match run_vl_caption(provider, vl_model, message, images).await {
        PreprocessOutcome::Skipped => (
            atomcode_coding::UserInput {
                text: message.to_string(),
                images: images.to_vec(),
            },
            None,
        ),
        PreprocessOutcome::Replaced { text, vl_model } => {
            let char_count = text.chars().count();
            let merged = if message.trim().is_empty() {
                format!("[图片内容（由 {vl_model} 识别）]\n{text}")
            } else {
                format!("{message}\n\n[图片内容（由 {vl_model} 识别）]\n{text}")
            };
            (
                atomcode_coding::UserInput {
                    text: merged,
                    images: Vec::new(),
                },
                Some(atomcode_coding::VisionNotice::Recognised {
                    vl_model,
                    char_count,
                }),
            )
        }
        PreprocessOutcome::Failed { reason } => (
            atomcode_coding::UserInput {
                text: fold_vl_failure(message),
                images: Vec::new(),
            },
            Some(atomcode_coding::VisionNotice::Failed { reason }),
        ),
    }
}

pub(crate) async fn preprocess_image_caption(
    config: &Config,
    supports_vision: bool,
    working_dir: &std::path::Path,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    session_id: Option<&str>,
    message: &str,
    images: &[ImageContent],
) -> String {
    preprocess_image_input(
        config,
        supports_vision,
        working_dir,
        telemetry,
        session_id,
        message,
        images,
    )
    .await
    .0
    .text
}

/// Fold the `[图片识别失败]` marker into a caption (VL build/stream failure). The
/// marker string is byte-identical to the CLI `apply_outcome` failure path so
/// `text_carries_vl_caption` + tuix `split_live_inputs` pair images correctly.
fn fold_vl_failure(message: &str) -> String {
    if message.trim().is_empty() {
        "[图片识别失败]".to_string()
    } else {
        format!("{message}\n\n[图片识别失败]")
    }
}

/// Whether `text` is a VL-preprocessed caption produced by [`preprocess_image_caption`]
/// (image described, or recognition failed) rather than the user's own words. The two
/// markers are the canonical signal the daemon uses to decide the raw image must NOT
/// reach a text-only model. Centralized so the runtime-strip and the webui-echo split
/// agree on one definition instead of open-coding the marker strings at each site.
fn text_carries_vl_caption(text: &str) -> bool {
    text.contains("[图片内容（由") || text.contains("[图片识别失败]")
}

/// Stash a VL-preprocessed submission's ORIGINAL images into the session's display-only
/// sidecar so another client (or a page refresh) re-attaches the thumbnail. No-op unless
/// the runtime text carries a VL caption — i.e. the image was stripped from the model
/// conversation, leaving the persisted snapshot image-less — AND at least one image exists.
/// Both the `/live` and `/chat` VL-strip paths call this; the `/chat` path previously
/// skipped it, so reloading clients (other users) saw only the "missing image" placeholder.
fn stash_vl_display_images(
    working_dir: &std::path::Path,
    session_id: &str,
    runtime_text: &str,
    original_images: &[ImageContent],
) {
    if !text_carries_vl_caption(runtime_text) || original_images.is_empty() {
        return;
    }
    // Ensure the project sessions dir exists: `append_display_images` is best-effort and
    // never creates it, and on `/chat` a brand-new session's first turn can reach here
    // before any snapshot save has created the dir — without this the sidecar write would
    // silently fail and the image would still be lost after refresh.
    let _ = std::fs::create_dir_all(
        atomcode_capabilities::session::SessionManager::for_project(working_dir).root(),
    );
    let display: Vec<crate::ImageData> = original_images
        .iter()
        .map(|image| crate::ImageData {
            media_type: image.media_type.clone(),
            data: image.data.clone(),
            missing: false,
        })
        .collect();
    crate::append_display_images(working_dir, session_id, display);
}

/// 对 live 输入做视觉预处理：主模型不支持视觉时，用 VL 模型把图片转文字拼进 caption
/// （原图始终保留在 MultiPart 里用于缩略图渲染）。与 `/chat` 路径共享
/// [`preprocess_image_caption`]；任何 config/provider 加载失败都降级为原文，不阻断发送。
/// `provider_name` 为本轮已解析的 selection id；显式能力覆盖优先，Auto
/// 才回退到模型名启发式判断。
async fn preprocess_live_caption(
    message: &str,
    images: &[ImageContent],
    provider_name: Option<&str>,
    session_id: Option<&str>,
    working_dir: &std::path::Path,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
) -> String {
    if images.is_empty() {
        return message.to_string();
    }
    let config = match Config::load(&Config::default_path()) {
        Ok(c) => c,
        Err(_) => return message.to_string(),
    };
    // The resolved model capability decides whether preprocessing is needed; the
    // one-off VL request rides `session_id` onto the same upstream account.
    let name = resolve_provider_name(&config, provider_name);
    let supports_vision = match config.provider_config_for_selection(&name) {
        Some(pc) => pc.accepts_images(),
        None => return message.to_string(),
    };
    preprocess_image_caption(
        &config,
        supports_vision,
        working_dir,
        telemetry,
        session_id,
        message,
        images,
    )
    .await
}

pub(crate) async fn live_message(
    State(state): State<AppState>,
    Extension(_client_mode): Extension<atomcode_telemetry::SessionMode>,
    Json(req): Json<LiveMessageReq>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    let sid = parse_session_id(req.session_id);
    let requested_provider = req.provider.clone();
    let bootstrap_provider = requested_provider
        .clone()
        .unwrap_or_else(live_current_provider);
    let join = match crate::native_live::ensure_headless_runtime(
        live_current_working_dir(&working_dir),
        state.telemetry.clone(),
        bootstrap_provider,
        native_runtime_mode(live_current_approval_mode()),
        sid,
    )
    .await
    {
        Ok(join) => join,
        Err(error) => {
            return Json(serde_json::json!({ "accepted": false, "error": error }));
        }
    };
    let active_provider = join.binding.provider.clone();
    let mut provider_name = active_provider.clone();
    let mut provider_change_applied = true;
    if let Some(requested_provider) = requested_provider {
        let config = match Config::load(&Config::default_path()) {
            Ok(config) => config,
            Err(error) => {
                return Json(serde_json::json!({
                    "accepted": false,
                    "error": format!("load provider config failed: {error}"),
                }));
            }
        };
        if !config.selection_exists(&requested_provider) {
            return Json(serde_json::json!({
                "accepted": false,
                "error": format!("provider {requested_provider:?} not found"),
            }));
        }
        let requested_fingerprint =
            match crate::native_live::provider_fingerprint(&config, &requested_provider) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    return Json(serde_json::json!({ "accepted": false, "error": error }));
                }
            };
        if provider_reload_required(
            &active_provider,
            &join.binding.provider_fingerprint,
            &requested_provider,
            &requested_fingerprint,
        ) {
            let runtime_config = chat_runtime_config(
                &config,
                &requested_provider,
                &join.binding.working_dir,
                state.telemetry.clone(),
            );
            let next = crate::kernel_runtime::coding_config_from_runtime(&runtime_config);
            match crate::native_live::reload_provider(&join.binding, next, requested_fingerprint)
                .await
            {
                Ok(_) => {
                    provider_name = requested_provider;
                }
                // A turn is active, so this /live/message is a STEER that folds into
                // the current turn — which cannot switch providers. Accept the steer
                // with the active provider and tell the client the optimistic provider
                // selection was not applied. We deliberately do not claim this is
                // queued: a later idle request must explicitly switch again.
                Err(crate::live_hub::HubError::ActiveTurn) => {
                    provider_change_applied = false;
                }
                Err(error) => {
                    return Json(serde_json::json!({
                        "accepted": false,
                        "active_turn": false,
                        "error": format!("provider reload rejected: {error:?}"),
                    }));
                }
            }
        } else {
            provider_name = requested_provider;
        }
    }
    let original_images: Vec<ImageContent> = req
        .images
        .into_iter()
        .map(|image| ImageContent {
            media_type: image.media_type,
            data: image.data,
        })
        .collect();
    let runtime_text = preprocess_live_caption(
        &req.message,
        &original_images,
        Some(&provider_name),
        Some(&join.binding.session_id),
        &join.binding.working_dir,
        state.telemetry.clone(),
    )
    .await;
    // VL preprocessing produced a caption ⇒ the runtime strips the image from the
    // conversation (it must never re-enter model context — see estimate_tokens). Stash the
    // originals in the display-only sidecar so a page refresh re-attaches the thumbnail.
    stash_vl_display_images(
        &join.binding.working_dir,
        &join.binding.session_id,
        &runtime_text,
        &original_images,
    );
    let (runtime_input, echo_input) = split_live_inputs(req.message, original_images, runtime_text);
    match crate::native_live::submit_confirmed_with_echo(
        runtime_input,
        echo_input,
        req.client_input_id,
    )
    .await
    {
        Ok(atomcode_coding::SubmitReceipt::Started {
            generation,
            turn_id,
        }) => Json(serde_json::json!({
            "accepted": true,
            "disposition": "started",
            "generation": generation,
            "turn_id": turn_id,
            "provider": provider_name,
            "provider_change_applied": provider_change_applied,
        })),
        Ok(atomcode_coding::SubmitReceipt::Steered {
            generation,
            turn_id,
        }) => Json(serde_json::json!({
            "accepted": true,
            "disposition": "steered",
            "generation": generation,
            "turn_id": turn_id,
            "provider": provider_name,
            "provider_change_applied": provider_change_applied,
        })),
        Err(error) => Json(serde_json::json!({
            "accepted": false,
            "error": format!("live submit rejected: {error:?}"),
        })),
    }
}

/// Split a submitted live message into the input fed to the model (`runtime`) vs the
/// input echoed to the live view (`echo`). BOTH keep the user's original image: the
/// runtime conversation must carry it so the image PERSISTS and reappears after a page
/// refresh (previously the sync path stripped it here, so the saved session had no
/// image). The raw bytes never reach a text-only model anyway — the provider adapter
/// degrades images at the wire when the model lacks vision (openai_compat
/// `supports_vision`). The two inputs differ only in TEXT: the runtime gets the VL
/// caption (the image description the text model needs) while the echo keeps the
/// user's ORIGINAL words, so the machine caption never overwrites what the user typed.
///
/// Every shipped adapter receives the same resolved `supports_vision` capability and
/// degrades images before constructing a text-only wire request.
fn split_live_inputs(
    message: String,
    original_images: Vec<atomcode_kernel::message::ImageContent>,
    runtime_text: String,
) -> (atomcode_coding::UserInput, atomcode_coding::UserInput) {
    (
        atomcode_coding::UserInput {
            text: runtime_text,
            images: original_images.clone(),
        },
        atomcode_coding::UserInput {
            text: message,
            images: original_images,
        },
    )
}

/// POST /live/stop — cancel the turn shared by the TUI and synchronized webui tabs.
pub(crate) async fn live_stop() -> impl IntoResponse {
    let accepted = crate::native_live::cancel_confirmed().await.is_ok();
    Json(serde_json::json!({ "accepted": accepted }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveSwitchSessionReq {
    pub session_id: String,
}

/// POST /live/switch_session — webui 切到「已存在」的会话时广播会话切换，
/// 让同进程 sync 模式的 TUI 跟随加载该会话（含历史）。
///
/// 目标会话在当前 live binding 的 project bucket 内精确定位，并在
/// legacy 收敛后将同一 lease 交给 CodingRuntime。无绑定 runtime 时明确
/// 返回拒绝，不隐式创建另一条执行路径。
pub(crate) async fn live_switch_session_endpoint(
    State(state): State<AppState>,
    Json(req): Json<LiveSwitchSessionReq>,
) -> impl IntoResponse {
    match crate::native_live::resume_session(req.session_id).await {
        Ok(changed) => {
            crate::update_project_state(&mut *state.project.write().await, &changed.working_dir);
            Json(serde_json::json!({ "ok": true }))
        }
        Err(error) => {
            let active_turn = matches!(error, crate::live_hub::HubError::ActiveTurn);
            Json(serde_json::json!({
                "ok": false,
                "active_turn": active_turn,
                "error": format!("session switch rejected: {error:?}"),
            }))
        }
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveProviderReq {
    pub provider: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// POST /live/provider — webui 切换模型即时同步。
///
/// 与"发送消息才带 provider"不同，下拉框一变就调本端点，让对端立即跟随而无需先发消息。
/// 该端点仍是 live runtime 的即时切换接口；TUI `/model` 另会更新新会话默认值。
pub(crate) async fn live_provider(
    State(state): State<AppState>,
    Json(req): Json<LiveProviderReq>,
) -> impl IntoResponse {
    let working_dir = { state.project.read().await.working_dir.clone() };
    let config = match Config::load(&Config::default_path()) {
        Ok(config) => config,
        Err(error) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("load provider config failed: {error}"),
            }));
        }
    };
    if !config.selection_exists(&req.provider) {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("provider {:?} not found", req.provider),
        }));
    }
    let requested_session_id = parse_session_id(req.session_id);
    let join = match crate::native_live::join_for_provider(requested_session_id.as_deref()) {
        Ok(join) => join,
        Err(crate::live_hub::HubError::Unbound) => {
            match crate::native_live::ensure_headless_runtime(
                live_current_working_dir(&working_dir),
                state.telemetry.clone(),
                req.provider.clone(),
                native_runtime_mode(live_current_approval_mode()),
                requested_session_id,
            )
            .await
            {
                Ok(join) => join,
                Err(error) => {
                    return Json(serde_json::json!({
                        "ok": false,
                        "error": error,
                    }));
                }
            }
        }
        Err(error) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("provider session rejected: {error:?}"),
            }));
        }
    };
    let requested_fingerprint =
        match crate::native_live::provider_fingerprint(&config, &req.provider) {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                return Json(serde_json::json!({ "ok": false, "error": error }));
            }
        };
    if !provider_reload_required(
        &join.binding.provider,
        &join.binding.provider_fingerprint,
        &req.provider,
        &requested_fingerprint,
    ) {
        return Json(serde_json::json!({ "ok": true }));
    }
    let runtime_config = chat_runtime_config(
        &config,
        &req.provider,
        &join.binding.working_dir,
        state.telemetry.clone(),
    );
    match crate::native_live::reload_provider(
        &join.binding,
        crate::kernel_runtime::coding_config_from_runtime(&runtime_config),
        requested_fingerprint,
    )
    .await
    {
        Ok(_) => Json(serde_json::json!({ "ok": true })),
        // A turn is running: reassembling the provider would hard-kill it and drop
        // the interrupted turn's context. Surface a distinct flag so the client can
        // revert its optimistic selection and tell the user to stop the turn first.
        Err(crate::live_hub::HubError::ActiveTurn) => Json(serde_json::json!({
            "ok": false,
            "active_turn": true,
            "error": "a turn is running; stop it before switching the model",
        })),
        Err(error) => Json(serde_json::json!({
            "ok": false,
            "error": format!("provider reload rejected: {error:?}"),
        })),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveModeReq {
    /// "build" | "accept_edits" | "bypass" | "plan"
    pub mode: ApprovalMode,
}

#[derive(serde::Serialize)]
pub(crate) struct ApprovalModeResp {
    pub ok: bool,
    pub mode: ApprovalMode,
}

pub(crate) async fn approval_mode_get() -> impl IntoResponse {
    Json(ApprovalModeResp {
        ok: true,
        mode: live_current_approval_mode(),
    })
}

async fn apply_live_mode(mode: ApprovalMode) -> bool {
    let accepted = match crate::native_live::binding() {
        Ok(_) => crate::native_live::set_mode(native_runtime_mode(mode))
            .await
            .is_ok(),
        Err(_) => true,
    };
    if accepted {
        live_set_mode(mode);
    }
    accepted
}

pub(crate) async fn approval_mode_set(Json(req): Json<LiveModeReq>) -> impl IntoResponse {
    let ok = apply_live_mode(req.mode).await;
    Json(ApprovalModeResp {
        ok,
        mode: live_current_approval_mode(),
    })
}

/// POST /live/mode — webui 底栏「模式」pill 切换审批模式
/// （build / accept_edits / bypass / plan）。
///
/// 更新进程级 LIVE_APPROVAL_MODE；若当前已有 live 会话，则广播 ModeChanged 让
/// 其他 webui tab / TUI 实时跟随。没有 live 会话时不为一次普通模式切换创建会话。
/// 下一轮实际用哪个 PermissionDecider 由 run_turn 读 LIVE_APPROVAL_MODE 决定。
/// 模式是运行时会话状态，不写入 config（与 provider 持久化为默认不同）——避免
/// Auto（wire: bypass）这种危险态被静默持久化。
pub(crate) async fn live_mode(Json(req): Json<LiveModeReq>) -> impl IntoResponse {
    let ok = apply_live_mode(req.mode).await;
    Json(ApprovalModeResp {
        ok,
        mode: live_current_approval_mode(),
    })
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveReasoningEffortReq {
    /// 目标 provider；None 时取当前默认 provider。
    #[serde(default)]
    pub provider: Option<String>,
    /// "low" | "medium" | "high" | "max" | null（API 默认）。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

/// POST /live/reasoning_effort — webui 设置当前模型实例的 reasoning_effort。
///
/// 与 /live/provider 同源：持久化进目标 provider 的 `config.reasoning_effort`，
/// 下一轮 turn 经 `build_turn_parts` → `create_provider` 自动生效——live 与
/// /chat 两条路径都现读 config，故两端都会跟随。模型实例必须由配置或内置
/// CodingPlan 能力声明支持；服务端同时校验取值。
pub(crate) async fn live_reasoning_effort(
    State(state): State<AppState>,
    Json(req): Json<LiveReasoningEffortReq>,
) -> impl IntoResponse {
    let effort = match req.reasoning_effort.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(v) if v.eq_ignore_ascii_case("low") => Some("low".to_string()),
        Some(v) if v.eq_ignore_ascii_case("medium") => Some("medium".to_string()),
        Some(v) if v.eq_ignore_ascii_case("high") => Some("high".to_string()),
        Some(v) if v.eq_ignore_ascii_case("max") => Some("max".to_string()),
        Some(other) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("invalid reasoning_effort: {other}"),
                })),
            )
                .into_response();
        }
    };
    let store = atomcode_config::ConfigStore::default_store();
    let requested = req.provider;
    let mut target = String::new();
    let mut previous_effort = None;
    let mut provider_missing = false;
    let mut level_disallowed = false;
    let commit = match store.update(|config| {
        target = requested
            .clone()
            .or_else(|| config.effective_model_selection())
            .unwrap_or_default();
        let builtin_effort = atomcode_config::config::is_codingplan_provider_name(&target)
            && config
                .provider_config_for_selection(&target)
                .is_some_and(|p| p.model.eq_ignore_ascii_case("deepseek-v4-flash"));
        // Reject a concrete level the endpoint does not expose. The webui filters
        // its dropdown, but a stale/rogue client could still POST a hidden level;
        // without this it would persist and reach the wire.
        if let Some(level) = effort.as_deref() {
            let levels = config
                .provider_config_for_selection(&target)
                .and_then(|p| p.reasoning_effort_levels);
            if atomcode_config::config::clamp_effort_to_levels(Some(level), levels.as_deref())
                .is_none()
            {
                level_disallowed = true;
                anyhow::bail!("reasoning_effort {level:?} not supported by {target:?}");
            }
        }
        // Schema-aware write: new-schema models live in `[models.*]`, legacy in
        // `[providers.*]`.
        let found = config.update_selection_reasoning(&target, |r| {
            previous_effort = r.reasoning_effort.clone();
            let keep_capability = previous_effort.is_some() || builtin_effort;
            *r.reasoning_effort = effort
                .clone()
                .or_else(|| keep_capability.then(|| "auto".to_string()));
        });
        if !found {
            provider_missing = true;
            anyhow::bail!("provider {target:?} not found");
        }
        Ok(())
    }) {
        Ok(commit) => commit,
        Err(_) if provider_missing => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("provider {target:?} not found"),
                })),
            )
                .into_response();
        }
        Err(_) if level_disallowed => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("reasoning_effort not supported by {target:?}"),
                })),
            )
                .into_response();
        }
        Err(error) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("save provider config failed: {error}"),
                })),
            )
                .into_response();
        }
    };
    let config = commit.snapshot.config.clone();

    if let Ok(binding) = crate::native_live::binding() {
        let runtime_config = chat_runtime_config(
            &config,
            &target,
            &binding.working_dir,
            state.telemetry.clone(),
        );
        let reload_result = match crate::native_live::provider_fingerprint(&config, &target) {
            Ok(fingerprint) => {
                crate::native_live::reload_provider(
                    &binding,
                    crate::kernel_runtime::coding_config_from_runtime(&runtime_config),
                    fingerprint,
                )
                .await
            }
            Err(error) => Err(crate::live_hub::HubError::RuntimeRejected(error)),
        };
        if let Err(error) = reload_result {
            // Roll back the persisted effort (the reload that would apply it was
            // refused). Surface the same active_turn flag as /live/provider.
            let active_turn = matches!(error, crate::live_hub::HubError::ActiveTurn);
            let rollback_error =
                match store.update_if_revision(&commit.snapshot.revision, |config| {
                    config.update_selection_reasoning(&target, |r| {
                        *r.reasoning_effort = previous_effort.clone();
                    });
                    Ok(())
                }) {
                    Ok(Some(_)) | Ok(None) => None,
                    Err(error) => Some(error.to_string()),
                };
            return Json(serde_json::json!({
                "ok": false,
                "active_turn": active_turn,
                "error": match rollback_error {
                    Some(rollback) => format!(
                        "provider reload rejected: {error:?}; config rollback failed: {rollback}"
                    ),
                    None => format!("provider reload rejected: {error:?}"),
                },
            }))
            .into_response();
        }
    }

    Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct LivePermissionReq {
    pub decision: String, // "allow" | "deny" | "always_allow" | "allow_persist"
    /// Full MCP tool name (`mcp__{server}__{tool}`); required for `allow_persist`.
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// POST /live/permission — Deliver a permission decision for a pending live-session tool-approval
/// request. The hub correlates the response with the pending native request.
///
/// Decision mapping mirrors /chat/permission:
///   "allow"        → PermissionDecision::AllowOnce
///   "always_allow" → PermissionDecision::AllowAlways (persisted for the session)
///   anything else  → PermissionDecision::Deny
pub(crate) async fn live_permission(
    State(state): State<AppState>,
    Json(req): Json<LivePermissionReq>,
) -> impl IntoResponse {
    use atomcode_capabilities::tools::{parse_permission_decision, PermissionDecision};
    let decision = if req.decision == "allow_persist" {
        if let Some(full) = req.tool_name.as_deref() {
            let reg = state.mcp_registry.read().await.clone();
            if let Some((server, tool)) = reg.split_tool_name(full).await {
                let project_dir = state.project.read().await.working_dir.clone();
                if let Err(e) = atomcode_capabilities::mcp::config::add_auto_approved_tool(
                    &project_dir,
                    &server,
                    &tool,
                ) {
                    tracing::warn!("[permission] persist autoApprove failed: {e}");
                }
                reg.mark_tool_auto_approved(full);
            }
        }
        PermissionDecision::AllowOnce
    } else {
        parse_permission_decision(&req.decision)
    };
    let response = match decision {
        PermissionDecision::AllowOnce => atomcode_capabilities::tools::ApprovalResponse::allow(),
        PermissionDecision::AllowAlways => {
            atomcode_capabilities::tools::ApprovalResponse::allow_always()
        }
        _ => atomcode_capabilities::tools::ApprovalResponse::deny(),
    };
    let value = serde_json::to_value(response).unwrap_or(serde_json::Value::Null);
    let ok = crate::native_live::respond_pending_kind_confirmed(
        atomcode_capabilities::tools::APPROVAL_KIND,
        value,
    )
    .await
    .is_ok();
    Json(serde_json::json!({ "accepted": ok }))
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct UserInputAnswerReq {
    pub request_id: u64,
    #[serde(default)]
    pub declined: bool,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
    /// Present for a multi-question batch: one response object per question. When set,
    /// the daemon responds `{ "responses": [...] }`; otherwise the flat single shape.
    #[serde(default)]
    pub responses: Option<serde_json::Value>,
}

impl UserInputAnswerReq {
    pub(crate) fn into_response_value(self) -> serde_json::Value {
        match self.responses {
            Some(responses) => serde_json::json!({ "responses": responses }),
            None => serde_json::json!({
                "declined": self.declined,
                "selected": self.selected,
                "text": self.text,
            }),
        }
    }
}

/// POST /live/user-input — Deliver the user's answer to a pending `request_user_input`
/// question raised by the agent, correlated by native request id.
///
/// Request body: `{ "request_id": u64, "declined": bool, "selected": [string], "text": string|null }`
/// Response: `{ "accepted": bool }` — false if there is no live session or no pending request
/// with that id.
pub(crate) async fn live_user_input(
    State(_state): State<AppState>,
    Json(req): Json<UserInputAnswerReq>,
) -> impl IntoResponse {
    // Batch answer (webui stepper) → `{ "responses": [...] }`; single → the flat shape.
    let request_id = req.request_id;
    let value = req.into_response_value();
    match crate::native_live::respond_confirmed(request_id, value).await {
        Ok(()) => axum::Json(serde_json::json!({ "accepted": true })),
        Err(error) => axum::Json(serde_json::json!({
            "accepted": false,
            "error": format!("user input request was not accepted: {error:?}"),
        })),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct PolicyInterventionResolutionReq {
    pub intervention_id: u64,
    pub action: atomcode_kernel::event::PolicyRecoveryAction,
}

/// Resolve a pending security intervention without creating model input.
pub(crate) async fn live_policy_intervention_resolution(
    State(_state): State<AppState>,
    Json(req): Json<PolicyInterventionResolutionReq>,
) -> impl IntoResponse {
    match crate::native_live::resolve_policy_intervention(req.intervention_id, req.action).await {
        Ok(()) => axum::Json(serde_json::json!({ "accepted": true })),
        Err(error) => axum::Json(serde_json::json!({
            "accepted": false,
            "error": format!("policy intervention resolution was not accepted: {error:?}"),
        })),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveCommandReq {
    /// 形如 `/status` 的斜杠命令行（带不带前导 `/` 都接受）。
    pub command: String,
}

/// POST /live/command —— 手机 App 请求桌面 TUI 执行一条斜杠命令。
/// 白名单（只读信息类）在 TUI 侧校验；输出经 /live 的 `command_output` 事件
/// 广播回来。返回 `{"accepted": bool}`：false 表示没有 TUI 附着（headless），
/// 命令无人执行。
pub(crate) async fn live_command(
    State(_state): State<AppState>,
    Json(req): Json<LiveCommandReq>,
) -> impl IntoResponse {
    let line = req.command.trim().to_string();
    let ok = !line.is_empty() && crate::native_live::send_remote_command(line);
    Json(serde_json::json!({ "accepted": ok }))
}

/// POST /live/cancel —— 取消当前正在运行的 turn(停止生成)。
/// 任一视图(手机 App「停止」/ webui / TUI)都可调用,先到先停。
/// 返回 `{"cancelled": bool}`:false 表示当前没有运行中的 turn。
pub(crate) async fn live_cancel(State(_state): State<AppState>) -> impl IntoResponse {
    let cancelled = crate::native_live::cancel_confirmed().await.is_ok();
    Json(serde_json::json!({ "cancelled": cancelled }))
}

#[derive(serde::Deserialize)]
pub(crate) struct LiveGoalReq {
    pub condition: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// POST /live/goal/start —— start the native CodingRuntime goal controller.
pub(crate) async fn live_goal_start(
    State(state): State<AppState>,
    Json(req): Json<LiveGoalReq>,
) -> impl IntoResponse {
    let condition = req.condition.unwrap_or_default().trim().to_string();
    if condition.is_empty() {
        return Json(serde_json::json!({"accepted": false, "error": "goal condition is empty"}));
    }
    // When the TUI owns the embedded runtime, route through its command
    // boundary as well. The TUI keeps a presentation/input Goal state of its
    // own; dispatching only a Runtime control would make App and TUI diverge
    // (App enters Goal, but the next TUI submit is ordinary text).
    if crate::native_live::embedded_binding().is_some() {
        let command = format!("/goal {condition}");
        let accepted = crate::native_live::send_remote_command(command);
        return Json(serde_json::json!({"accepted": accepted, "delegated": accepted}));
    }
    let working_dir = { state.project.read().await.working_dir.clone() };
    let sid = parse_session_id(req.session_id);
    if let Err(error) = crate::native_live::ensure_headless_runtime(
        live_current_working_dir(&working_dir),
        state.telemetry.clone(),
        live_current_provider(),
        native_runtime_mode(live_current_approval_mode()),
        sid,
    )
    .await
    {
        return Json(serde_json::json!({"accepted": false, "error": error}));
    }
    let accepted =
        crate::native_live::dispatch(atomcode_coding::DriverCommand::StartGoal(condition)).is_ok();
    Json(serde_json::json!({"accepted": accepted}))
}

/// POST /live/goal/stop —— stop the native Goal controller (not just one turn).
pub(crate) async fn live_goal_stop(State(_state): State<AppState>) -> impl IntoResponse {
    if crate::native_live::embedded_binding().is_some() {
        let accepted = crate::native_live::send_remote_command("/goal clear".into());
        return Json(serde_json::json!({"accepted": accepted, "delegated": accepted}));
    }
    let accepted = crate::native_live::dispatch(atomcode_coding::DriverCommand::StopGoal).is_ok();
    Json(serde_json::json!({"accepted": accepted}))
}

/// POST /live/goal/arm — synchronize the mobile composer's pre-send Goal mode
/// with an attached TUI. No condition is known until either side submits text.
pub(crate) async fn live_goal_arm(State(_state): State<AppState>) -> impl IntoResponse {
    if crate::native_live::embedded_binding().is_some() {
        let accepted = crate::native_live::send_remote_command("/goal __app_arm".into());
        return Json(serde_json::json!({"accepted": accepted}));
    }
    Json(serde_json::json!({"accepted": true}))
}

/// POST /live/compact —— webui/手机端在 sync 模式请求对共享实时运行时执行一次
/// 手动压缩。派发 `DriverCommand::Compact(None)` 到 live hub；压缩结果经既有的
/// `NativeLiveWireProjector`（CompactionFinished → Warning）回流到各视图。
/// 返回 `{"accepted": bool}`：false 表示当前没有绑定的实时运行时（无可压缩对象）。
pub(crate) async fn live_compact(State(_state): State<AppState>) -> impl IntoResponse {
    let accepted =
        crate::native_live::dispatch(atomcode_coding::DriverCommand::Compact(None)).is_ok();
    Json(serde_json::json!({ "accepted": accepted }))
}

/// POST /live/mcp/trust — Trust the current project so its `.mcp.json` servers
/// are allowed to connect on the next turn. Rebuilds the serving MCP registry
/// so newly-allowed servers start connecting immediately.
///
/// Response on success: `{"ok": true, "trusted": true}`
/// Response on failure: HTTP 500 + `{"ok": false, "error": "..."}`
pub(crate) async fn live_mcp_trust(State(state): State<AppState>) -> impl IntoResponse {
    let fallback = { state.project.read().await.working_dir.clone() };
    let working_dir = live_current_working_dir(&fallback);
    match atomcode_capabilities::mcp::trust::trust_project(&working_dir) {
        Ok(()) => {
            let new_registry = Arc::new(McpRegistry::from_config_background(&working_dir));
            crate::replace_project_mcp_registry(&state, &working_dir, new_registry).await;
            // Re-prepare the persistent native runtime so it mounts the newly
            // trusted project servers immediately. Best-effort: before the first
            // turn there is no runtime yet, and its first prepare reads trust
            // from disk directly.
            let reloaded = match crate::native_live::binding() {
                Ok(_) => crate::native_live::reload_capabilities().await.is_ok(),
                Err(_) => true,
            };
            Json(serde_json::json!({
                "ok": reloaded,
                "trusted": true,
            }))
            .into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;

    fn img(tag: &str) -> atomcode_kernel::message::ImageContent {
        atomcode_kernel::message::ImageContent {
            media_type: "image/png".into(),
            data: tag.into(),
        }
    }

    // A reloaded VL-stripped user message renders as a "missing image" placeholder
    // (see MessageInfo::from_kernel). Other clients only recover the thumbnail if the
    // ORIGINAL bytes were stashed to the display-only sidecar during the turn.
    fn missing_user_msg() -> crate::MessageInfo {
        crate::MessageInfo {
            role: "user".into(),
            content: "识别一下".into(),
            synthetic: false,
            internal_origin: None,
            tool_calls: None,
            tool_result: None,
            artifacts: None,
            images: Some(vec![crate::ImageData {
                media_type: "image/png".into(),
                data: String::new(),
                missing: true,
            }]),
            created_at: None,
        }
    }

    #[test]
    fn vl_stripped_image_survives_reload_via_display_sidecar() {
        // Repro of "image lost after refresh for other users": on a VL-strip turn the
        // persisted user message is image-less, so the ORIGINAL image MUST be stashed to
        // the display-only sidecar for a fresh reload to refill it. The /live path did
        // this; the /chat path did NOT — both now share `stash_vl_display_images`.
        let dir = tempfile::tempdir().unwrap();
        let wd = dir.path();
        let sid = "sess-vl-reload";
        // NOTE: the sessions dir is deliberately NOT pre-created — stash_vl_display_images
        // must create it itself (a brand-new session's first /chat turn reaches the stash
        // before any snapshot save has made the dir).

        // BUG: no sidecar written → a fresh reload keeps the "missing" placeholder.
        let mut before = vec![missing_user_msg()];
        crate::attach_display_images(&mut before, wd, sid);
        assert!(
            before[0].images.as_ref().unwrap()[0].missing,
            "no sidecar → other clients still see the missing placeholder"
        );

        // FIX: the shared helper stashes the original bytes (VL caption + image present).
        stash_vl_display_images(
            wd,
            sid,
            "识别一下\n\n[图片内容（由 vl 识别）]\na cat",
            &[img("REAL-BYTES")],
        );

        let mut after = vec![missing_user_msg()];
        crate::attach_display_images(&mut after, wd, sid);
        let refilled = &after[0].images.as_ref().unwrap()[0];
        assert!(!refilled.missing, "sidecar present → placeholder refilled");
        assert_eq!(refilled.data, "REAL-BYTES");

        // Gating: no VL caption OR no image → nothing stashed (don't pollute the sidecar).
        let sid2 = "sess-no-vl";
        stash_vl_display_images(wd, sid2, "plain text, no caption", &[img("X")]);
        stash_vl_display_images(wd, sid2, "[图片内容（由 vl 识别）] no image", &[]);
        let mut plain = vec![missing_user_msg()];
        crate::attach_display_images(&mut plain, wd, sid2);
        assert!(
            plain[0].images.as_ref().unwrap()[0].missing,
            "no VL caption / no image → nothing stashed → stays missing"
        );
    }

    #[test]
    fn split_live_inputs_keeps_image_in_runtime_for_persistence_when_vl_preprocessed() {
        // A text-only model preprocessed the image into a caption. The RUNTIME gets the
        // caption text BUT keeps the original image so it persists (survives a refresh);
        // the adapter degrades the image at the wire for the text-only model. The ECHO
        // keeps the user's ORIGINAL text + image so the caption never overwrites the
        // user's message. (Fixes: image gone after refresh + caption-overwrite.)
        let (runtime, echo) = split_live_inputs(
            "look at this".into(),
            vec![img("orig-bytes")],
            "look at this\n\n[图片内容（由 vl 识别）]\na chart".into(),
        );
        assert_eq!(
            runtime.text,
            "look at this\n\n[图片内容（由 vl 识别）]\na chart"
        );
        assert_eq!(
            runtime.images,
            vec![img("orig-bytes")],
            "runtime conversation must KEEP the image so it persists across a refresh"
        );
        assert_eq!(
            echo.text, "look at this",
            "display must show the user's original text"
        );
        assert_eq!(
            echo.images,
            vec![img("orig-bytes")],
            "display must keep the image"
        );
    }

    #[test]
    fn split_live_inputs_keeps_image_for_vision_model_when_not_preprocessed() {
        // A vision model needs no caption: runtime_text == message, so the raw image
        // flows to BOTH the model and the echo unchanged.
        let (runtime, echo) = split_live_inputs("hi".into(), vec![img("orig-bytes")], "hi".into());
        assert_eq!(runtime.text, "hi");
        assert_eq!(runtime.images, vec![img("orig-bytes")]);
        assert_eq!(echo.text, "hi");
        assert_eq!(echo.images, vec![img("orig-bytes")]);
    }

    #[test]
    fn resolve_provider_name_prefers_override_then_default() {
        let mut config = Config::default();
        config.default_provider = "default-prov".to_string();

        // Explicit override wins.
        assert_eq!(resolve_provider_name(&config, Some("chosen")), "chosen");
        // No override → falls back to the config default.
        assert_eq!(resolve_provider_name(&config, None), "default-prov");
    }

    /// Trust round-trip at the daemon layer: trust_project → is_project_trusted → partition_by_trust
    /// clears blocked list.  Uses ATOMCODE_MCP_TRUST_STORE as the test seam so we never touch the
    /// developer's real trust store.
    #[test]
    #[serial_test::serial]
    fn mcp_trust_round_trip_clears_blocked() {
        use atomcode_capabilities::mcp::config::{
            McpConfigSource, McpServerConfig, McpTransportConfig,
        };
        use atomcode_capabilities::mcp::trust::{
            is_project_trusted, partition_by_trust, trust_project,
        };

        let store_dir = tempfile::tempdir().unwrap();
        // SAFETY: test seam; serial attribute prevents concurrent mutation.
        unsafe {
            std::env::set_var(
                "ATOMCODE_MCP_TRUST_STORE",
                store_dir.path().join("mcp_trust_daemon_test.json"),
            );
        }

        let proj = store_dir.path().join("fake-project");

        // Before trust: project-source server appears in blocked.
        let project_cfg = McpServerConfig {
            name: "untrusted-server".to_string(),
            disabled: false,
            config: McpTransportConfig::Stdio {
                command: "true".to_string(),
                args: vec![],
                env: Default::default(),
                timeout_ms: None,
            },
            source: McpConfigSource::Project,
            trust: false,
            auto_approve: vec![],
        };
        let part_before = partition_by_trust(vec![project_cfg.clone()], &proj);
        assert_eq!(
            part_before.blocked.len(),
            1,
            "untrusted project: server should be blocked"
        );
        assert!(part_before.allowed.is_empty());
        assert!(
            !is_project_trusted(&proj),
            "fresh store: project must be untrusted"
        );

        // Trust the project.
        trust_project(&proj).expect("trust_project must not fail");
        assert!(
            is_project_trusted(&proj),
            "after trust_project: project must be trusted"
        );

        // After trust: same config yields empty blocked.
        let part_after = partition_by_trust(vec![project_cfg], &proj);
        assert!(
            part_after.blocked.is_empty(),
            "trusted project: blocked must be empty"
        );
        assert_eq!(part_after.allowed.len(), 1);

        // Cleanup env so other serial tests see a clean state.
        unsafe { std::env::remove_var("ATOMCODE_MCP_TRUST_STORE") };
    }

    #[test]
    fn real_empty_terminal_snapshot_clears_the_conversation() {
        // Seed the buffer with a cold-summary synthetic (kernel encoding) + a real
        // user message; an empty authoritative terminal must wipe BOTH — inline cold
        // summaries are just messages now, so nothing survives an empty snapshot.
        let mut cold = Message::user(format!(
            "{}stale summary",
            atomcode_kernel::message::LEGACY_COLD_SUMMARY_PREFIX
        ));
        cold.synthetic = true;
        cold.internal_origin =
            Some(atomcode_kernel::message::LEGACY_COLD_SUMMARY_ORIGIN.to_string());
        let mut buffer = vec![cold, Message::user("cancelled prompt")];

        install_authoritative_terminal_snapshot(&mut buffer, SessionSnapshot::new(Vec::new()), &[]);

        assert!(buffer.is_empty());
        assert!(atomcode_kernel::message::cold_summaries_from_messages(&buffer).is_empty());
    }

    /// The webui `/live/mode` body + `mode`/`snapshot` SSE events serialize the
    /// mode as lowercase `build`/`accept_edits`/`bypass`/`plan`. The frontend `ApprovalMode`
    /// union depends on these EXACT strings — lock the wire contract.
    #[test]
    fn approval_mode_wire_strings_are_lowercase() {
        let cases = [
            (ApprovalMode::Build, "build"),
            (ApprovalMode::AcceptEdits, "accept_edits"),
            (ApprovalMode::Plan, "plan"),
            (ApprovalMode::Auto, "bypass"),
        ];
        for (mode, wire) in cases {
            // Serialize (used by Snapshot.mode + ModeChanged broadcast).
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(wire));
            // Deserialize (the `/live/mode` request body → LiveModeReq.mode).
            let back: ApprovalMode = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(back, mode);
        }
        // Default is Build (the safe interactive-approval mode).
        assert_eq!(ApprovalMode::default(), ApprovalMode::Build);
    }

    #[test]
    fn fallback_approval_is_closed_for_prompt_required_modes() {
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Plan),
            PermissionDecision::Deny
        ));
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::AcceptEdits),
            PermissionDecision::Deny
        ));
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Build),
            PermissionDecision::AllowOnce
        ));
        assert!(matches!(
            fallback_approval_decision(ApprovalMode::Auto),
            PermissionDecision::AllowOnce
        ));
    }

    #[test]
    fn native_runtime_mode_preserves_all_approval_modes() {
        let cases = [
            (ApprovalMode::Build, atomcode_coding::RuntimeMode::Build),
            (
                ApprovalMode::AcceptEdits,
                atomcode_coding::RuntimeMode::AcceptEdits,
            ),
            (ApprovalMode::Auto, atomcode_coding::RuntimeMode::Auto),
            (ApprovalMode::Plan, atomcode_coding::RuntimeMode::Plan),
        ];

        for (approval_mode, runtime_mode) in cases {
            assert_eq!(native_runtime_mode(approval_mode), runtime_mode);
        }
    }

    #[tokio::test]
    async fn approval_mode_get_returns_current_runtime_mode() {
        let _mode_guard = ScopedApprovalModeForTest::new();
        live_set_mode(ApprovalMode::Auto);

        let response = approval_mode_get().await.into_response();
        assert_eq!(response.status().as_u16(), 200);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read response body");
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("approval mode response json");

        assert_eq!(value, serde_json::json!({ "ok": true, "mode": "bypass" }));
    }

    /// Regression guard (2nd occurrence — see the `never eprintln` note near the
    /// top of this file). Under `/webui` the live path runs IN the TUI
    /// process, so a console print writes straight to the shared terminal and
    /// corrupts the TUI — a stray native-runtime startup diagnostic
    /// landed on the input line when a dir switch during sync spun up the live
    /// stack. Every diagnostic in this file must use the file-sink `ctrace!`.
    ///
    /// This scans our own source for the print-macro family (`print!` / `println!`
    /// / `eprint!` / `eprintln!`) plus `dbg!`, which cover the realistic
    /// regressions. It does NOT catch raw handle writes (`write!(io::stdout(), …)`)
    /// — those are left to the module-level `#![deny(clippy::print_stdout, …)]`
    /// and review, since a `stdout(`/`stderr(` substring scan false-positives on
    /// `Command::stdout(Stdio::…)` and friends. Backstop, not a proof.
    #[test]
    fn no_console_prints_in_live_path() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/live_api.rs"))
            .expect("read live_api.rs source");
        // Needles built at runtime so this test body doesn't match itself. The
        // "println" needle also catches the eprintln variant (it ends the same
        // way); the "print" needle catches the eprint variant; "dbg" catches the
        // one-keystroke debug print that writes to stderr.
        let needles = [
            format!("{}{}", "println", "!("),
            format!("{}{}", "print", "!("),
            format!("{}{}", "dbg", "!("),
        ];
        for (i, line) in src.lines().enumerate() {
            if let Some(hit) = needles.iter().find(|n| line.contains(n.as_str())) {
                panic!(
                    "console print (`{}`) at live_api.rs:{} — use ctrace! (file sink), \
                     never a console print: the /webui live path runs in the TUI process \
                     and any stdout/stderr write here corrupts the terminal. Line: {}",
                    hit,
                    i + 1,
                    line.trim(),
                );
            }
        }
    }

    // 回归：/live/message 必须解析显式 provider，但不接受 per-message mode 覆盖。
    #[test]
    fn live_message_parses_optional_provider() {
        // 带 provider 的请求体被解析。
        // `approval_mode` is deliberately ignored here: live approval mode is
        // global runtime state changed only through /approval_mode or /live/mode,
        // not a per-message override.
        let req: LiveMessageReq =
            serde_json::from_str(r#"{"message":"hi","provider":"openai","approval_mode":"plan"}"#)
                .unwrap();
        assert_eq!(req.provider.as_deref(), Some("openai"));

        // 不带 provider 的请求体默认 None。
        let req2: LiveMessageReq = serde_json::from_str(r#"{"message":"hi"}"#).unwrap();
        assert_eq!(req2.provider, None);
    }

    #[test]
    fn live_message_reloads_only_when_the_runtime_provider_identity_changes() {
        assert!(!provider_reload_required(
            "ds-gf",
            "fingerprint-a",
            "ds-gf",
            "fingerprint-a",
        ));
        assert!(provider_reload_required(
            "ds-gf",
            "fingerprint-a",
            "ds-gf",
            "fingerprint-b",
        ));
        assert!(provider_reload_required(
            "ds-gf",
            "fingerprint-a",
            "other",
            "fingerprint-a",
        ));
    }

    #[test]
    #[serial_test::serial]
    fn live_working_dir_updates_daemon_project_view() {
        let dir_a = std::path::PathBuf::from("/tmp/atomcode-test-a");
        let dir_b = std::path::PathBuf::from("/tmp/atomcode-test-b");

        // Initialize DAEMON_PROJECT with a test ProjectStateStore.
        let project_state = crate::ProjectState {
            working_dir: dir_a.clone(),
            previous_dir: None,
            recent_dirs: vec![dir_a.clone()],
            name: "test-a".to_string(),
        };
        let project_store = Arc::new(tokio::sync::RwLock::new(project_state));
        *crate::DAEMON_PROJECT.lock().unwrap() = Some(project_store.clone());

        live_set_working_dir(dir_b.clone());

        {
            let project = project_store.blocking_read();
            assert_eq!(project.working_dir, dir_b);
            assert_eq!(project.previous_dir.as_ref(), Some(&dir_a));
            assert_eq!(project.name, "atomcode-test-b");
            assert_eq!(project.recent_dirs, vec![dir_b.clone(), dir_a.clone()]);
        }

        *crate::DAEMON_PROJECT.lock().unwrap() = None;
    }

    // 回归：无图时视觉预处理是直通的——caption 原样返回，不触碰 config/网络。
    // （有图的 VL 流式路径覆盖在 atomcode_coding::vision::run_vl_caption 的单测里。）
    #[tokio::test]
    async fn preprocess_live_caption_is_passthrough_without_images() {
        // Disabled telemetry + throwaway dir: empty images short-circuit BEFORE
        // any provider build, so neither is exercised — just needed to type-check.
        let telemetry = atomcode_telemetry::Telemetry::init(
            atomcode_telemetry::config::ResolvedConfig {
                state: atomcode_telemetry::config::TelemetryState::Disabled("test"),
                endpoint: String::new(),
                atomcode_dir: std::env::temp_dir(),
            },
            "test".into(),
        );
        let out = preprocess_live_caption(
            "看下这个图片",
            &[],
            None,
            None,
            &std::env::temp_dir(),
            telemetry,
        )
        .await;
        assert_eq!(out, "看下这个图片");
    }

    #[test]
    fn live_runtime_config_parks_interactive_requests_without_timeout() {
        let telemetry = atomcode_telemetry::Telemetry::init(
            atomcode_telemetry::config::ResolvedConfig {
                state: atomcode_telemetry::config::TelemetryState::Disabled("test"),
                endpoint: String::new(),
                atomcode_dir: std::env::temp_dir(),
            },
            "test".into(),
        );
        let runtime = live_runtime_config(
            &atomcode_config::config::Config::default(),
            "missing-test-provider",
            &std::env::temp_dir(),
            telemetry,
        );
        assert!(runtime.interactive);
        assert_eq!(
            crate::kernel_runtime::coding_config_from_runtime(&runtime).request_timeout,
            None
        );
    }

    #[tokio::test]
    async fn chat_user_input_wait_degrades_to_null_at_driver_timeout() {
        let (_tx, rx) = tokio::sync::oneshot::channel();
        let value =
            await_chat_user_input_response(rx, Some(std::time::Duration::from_millis(1))).await;
        assert!(value.is_null());
    }

    #[tokio::test]
    async fn chat_user_input_wait_returns_the_correlated_answer() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(serde_json::json!({ "selected": ["Blue"] }))
            .unwrap();
        let value =
            await_chat_user_input_response(rx, Some(std::time::Duration::from_secs(1))).await;
        assert_eq!(value["selected"][0], "Blue");
    }

    #[test]
    fn restore_images_from_turn_base_preserves_history_user_display_payload() {
        let original_user = Message::user_with_images("识别图片内容", vec![img("aW1hZ2U=")]);
        let final_user =
            Message::user("识别图片内容\n\n[图片内容（由 vl-provider 识别）]\n一张图片");

        let messages = restore_images_from_turn_base(vec![final_user], &[original_user]);

        assert_eq!(messages[0].text, "识别图片内容");
        assert_eq!(messages[0].images.len(), 1);
        assert_eq!(messages[0].images[0].data, "aW1hZ2U=");
    }

    #[test]
    fn restore_images_from_turn_base_matches_user_turns_when_final_snapshot_has_system_prefix() {
        let original_user = Message::user_with_images("分析", vec![img("aW1hZ2U=")]);
        let final_messages = vec![
            Message::system("session context"),
            Message::system("memory"),
            Message::user("分析\n\n[图片内容（由 vl-provider 识别）]\n一张图片"),
            Message::assistant("done", Vec::new()),
        ];

        let messages = restore_images_from_turn_base(final_messages, &[original_user]);

        assert_eq!(messages[2].text, "分析");
        assert_eq!(messages[2].images.len(), 1);
        assert_eq!(messages[2].images[0].data, "aW1hZ2U=");
    }

    #[test]
    fn restore_images_from_turn_base_keeps_user_turn_ordinal_with_prior_text_user() {
        let prior_user = Message::user("上一轮问题");
        let image_user = Message::user_with_images("分析", vec![img("aW1hZ2U=")]);
        let final_messages = vec![
            Message::system("session context"),
            Message::user("上一轮问题"),
            Message::assistant("上一轮回答", Vec::new()),
            Message::user("分析\n\n[图片内容（由 vl-provider 识别）]\n一张图片"),
            Message::assistant("done", Vec::new()),
        ];

        let messages = restore_images_from_turn_base(final_messages, &[prior_user, image_user]);

        // The prior text-only user turn stays untouched (no images restored onto it).
        assert_eq!(messages[1].text, "上一轮问题");
        assert!(messages[1].images.is_empty());
        assert_eq!(messages[3].text, "分析");
        assert_eq!(messages[3].images.len(), 1);
        assert_eq!(messages[3].images[0].data, "aW1hZ2U=");
    }

    #[test]
    fn restore_images_from_turn_base_ignores_synthetic_user_ordinals() {
        let image_user = Message::user_with_images("分析图片", vec![img("aW1hZ2U=")]);
        let final_messages = vec![
            Message::synthetic_user("[Auto-read from error: src/main.rs]\nfn main() {}"),
            Message::user("分析图片\n\n[图片内容（由 vl-provider 识别）]\n一张图片"),
            Message::assistant("done", Vec::new()),
        ];

        let messages = restore_images_from_turn_base(
            final_messages,
            &[
                Message::synthetic_user("[Auto-read from error: src/main.rs]"),
                image_user,
            ],
        );

        // The synthetic user is skipped (not counted as a real user ordinal), so
        // its text is untouched and it never receives restored images.
        assert!(messages[0].synthetic);
        assert!(messages[0].text.contains("Auto-read"));
        assert!(messages[0].images.is_empty());
        assert_eq!(messages[1].text, "分析图片");
        assert_eq!(messages[1].images.len(), 1);
        assert_eq!(messages[1].images[0].data, "aW1hZ2U=");
    }

    #[test]
    fn native_live_projector_preserves_rate_limit_fields() {
        let mut projector = NativeLiveWireProjector::default();
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::RateLimited {
                    reset_at_display: "18:09".into(),
                    reset_label: "5h".into(),
                    secs_until_reset: Some(7200),
                    auto_resuming: false,
                    server_message: Some("provider quota exhausted".into()),
                }),
            ))
            .expect("rate limit must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "rate_limited");
        assert_eq!(json["reset_at_display"], "18:09");
        assert_eq!(json["reset_label"], "5h");
        assert_eq!(json["secs_until_reset"], 7200);
        assert_eq!(json["server_message"], "provider quota exhausted");
    }

    #[test]
    fn native_live_projector_preserves_policy_recovery_contract() {
        let mut projector = NativeLiveWireProjector::default();
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::PolicyIntervention {
                    intervention:
                        atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(),
                }),
            ))
            .expect("policy intervention must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "policy_intervention");
        assert_eq!(json["code"], "credential_shell_blocked");
        assert_eq!(json["actions"].as_array().map(Vec::len), Some(4));
        assert!(!json.to_string().contains("WECOM_WEBHOOK_URL"));
    }

    #[test]
    fn native_live_projector_exposes_policy_resolution() {
        let mut projector = NativeLiveWireProjector::default();
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::PolicyInterventionResolved {
                    intervention_id: 42,
                    action: atomcode_kernel::event::PolicyRecoveryAction::SkipStep,
                },
            ))
            .expect("policy resolution must reach every live client");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "policy_intervention_resolved");
        assert_eq!(json["intervention_id"], 42);
        assert_eq!(json["action"], "skip_step");
    }

    #[test]
    fn native_live_projector_preserves_the_authoritative_stop_reason() {
        for (reason, expected) in [
            (atomcode_kernel::event::StopReason::MaxRounds, "max_rounds"),
            (
                atomcode_kernel::event::StopReason::RepeatLoop,
                "repeat_loop",
            ),
            (
                atomcode_kernel::event::StopReason::ToolLoopDetected,
                "tool_loop_detected",
            ),
        ] {
            let mut projector = NativeLiveWireProjector::default();
            let wire = projector
                .project(crate::live_hub::LiveViewEvent::Runtime(
                    CodingRuntimeEvent::TurnFinished(atomcode_coding::TurnCompletion::Completed {
                        turn_id: 7,
                        reason,
                        snapshot: std::sync::Arc::new(
                            atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
                        ),
                        stats: atomcode_coding::RuntimeTurnStats::default(),
                    }),
                ))
                .expect("turn terminal must reach the live wire");
            let json = serde_json::to_value(wire).unwrap();
            assert_eq!(json["type"], "state");
            assert_eq!(json["running"], false);
            assert_eq!(json["stop_reason"], expected);
            assert!(json.get("message").is_none());
        }
    }

    #[test]
    fn native_live_projector_projects_runtime_stop_as_authoritative_state() {
        let mut projector = NativeLiveWireProjector::default();
        projector
            .tools
            .insert("call-1".into(), ("bash".into(), std::time::Instant::now()));
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::RuntimeStopped(atomcode_coding::RuntimeExit {
                    reason: atomcode_coding::RuntimeExitReason::OwnerStopped,
                    forced: false,
                }),
            ))
            .expect("runtime stop must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "state");
        assert_eq!(json["running"], false);
        assert_eq!(json["stop_reason"], "runtime_stopped");
        assert!(json["message"].as_str().is_some_and(|m| !m.is_empty()));
        assert!(projector.tools.is_empty());
    }

    #[test]
    fn native_live_projector_prioritizes_snapshot_failure_over_inner_stop_reason() {
        let mut projector = NativeLiveWireProjector::default();
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::TurnFinished(
                    atomcode_coding::TurnCompletion::SnapshotUnavailable {
                        turn_id: 7,
                        reason: atomcode_kernel::event::StopReason::Stopped,
                        error: atomcode_coding::RuntimeSnapshotError {
                            message: "snapshot failed".into(),
                        },
                        stats: atomcode_coding::RuntimeTurnStats::default(),
                    },
                ),
            ))
            .expect("snapshot failure must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "state");
        assert_eq!(json["running"], false);
        assert_eq!(json["stop_reason"], "snapshot_unavailable");
        assert_eq!(json["message"], "snapshot failed");
    }

    #[test]
    fn native_live_projector_exposes_only_typed_user_input_requests() {
        use atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND;

        let mut projector = NativeLiveWireProjector::default();
        let request = atomcode_coding::RuntimeRequest {
            id: 42,
            kind: REQUEST_USER_INPUT_KIND.into(),
            payload: serde_json::json!({
                "header": "Pick one",
                "question": "Red or blue?",
                "mode": "single",
                "options": [{ "label": "Red" }, { "label": "Blue" }]
            }),
            snapshot: None,
        };
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Request(request),
            ))
            .expect("typed request must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "user_input_request");
        assert_eq!(json["request_id"], 42);
        assert_eq!(json["mode"], "single");
        assert_eq!(json["options"][0]["label"], "Red");

        let unknown = atomcode_coding::RuntimeRequest {
            id: 43,
            kind: "unknown_future_kind".into(),
            payload: serde_json::Value::Null,
            snapshot: None,
        };
        assert!(projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Request(unknown),
            ))
            .is_none());

        let resolved = projector
            .project(crate::live_hub::LiveViewEvent::RequestResolved {
                request_id: 42,
                kind: REQUEST_USER_INPUT_KIND.into(),
            })
            .expect("typed request terminal must reach the live wire");
        assert_eq!(
            serde_json::to_string(&resolved).unwrap(),
            r#"{"type":"user_input_resolved","request_id":42}"#
        );
        assert!(projector
            .project(crate::live_hub::LiveViewEvent::RequestResolved {
                request_id: 43,
                kind: "unknown_future_kind".into(),
            })
            .is_none());
    }

    #[test]
    fn native_live_projector_exposes_exact_steered_inputs() {
        let mut projector = NativeLiveWireProjector::default();
        let wire = projector
            .project(crate::live_hub::LiveViewEvent::Steered {
                count: 1,
                inputs: vec![atomcode_kernel::event::SteeredInput {
                    text: "use the smaller fix".into(),
                    images: Vec::new(),
                }],
                client_input_ids: vec![Some("web-1".into())],
            })
            .expect("steer acknowledgement must reach the live wire");
        let json = serde_json::to_value(wire).unwrap();
        assert_eq!(json["type"], "steered");
        assert_eq!(json["count"], 1);
        assert_eq!(json["inputs"][0]["text"], "use the smaller fix");
        assert_eq!(json["inputs"][0]["images"], serde_json::json!([]));
        assert_eq!(json["client_input_ids"][0], "web-1");
    }

    #[test]
    fn native_live_projector_keeps_warning_and_tool_progress_distinct() {
        let mut projector = NativeLiveWireProjector::default();
        let warning = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::Warning(
                    "conversation compacted".into(),
                )),
            ))
            .expect("warning must reach the live wire");
        assert_eq!(
            serde_json::to_string(&warning).unwrap(),
            r#"{"type":"warning","message":"conversation compacted"}"#
        );

        let progress = projector
            .project(crate::live_hub::LiveViewEvent::Runtime(
                CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::ToolProgress {
                    call_id: "c1".into(),
                    message: "explore#4 · grep unwrap".into(),
                }),
            ))
            .expect("tool progress must reach the live wire");
        let json = serde_json::to_value(progress).unwrap();
        assert_eq!(json["type"], "tool_progress");
        assert_eq!(json["id"], "c1");
    }

    #[test]
    fn committed_compaction_event_exposes_exact_kernel_snapshot_messages() {
        use atomcode_coding::runtime::{CompactionCompletion, CompactionOutcome};
        use atomcode_kernel::message::{CompactTrigger, Message, SessionSnapshot};

        let mut kernel_message = Message::user("after compact");
        kernel_message.synthetic = true;
        let event = CodingRuntimeEvent::CompactionFinished {
            completion: CompactionCompletion::Completed(CompactionOutcome {
                trigger: CompactTrigger::Manual { focus: None },
                epoch: 1,
                removed_messages: 2,
                bytes_before: 100,
                bytes_after: 50,
                committed: true,
                estimated_tokens_before: 25,
                estimated_tokens_after: 12,
                committed_snapshot: Some(std::sync::Arc::new(SessionSnapshot::new(vec![
                    kernel_message,
                ]))),
            }),
        };

        let snapshot = committed_compaction_snapshot(&event)
            .expect("valid completion")
            .expect("committed snapshot");
        assert_eq!(snapshot.messages.len(), 1);
        assert!(snapshot.messages[0].synthetic);
        assert_eq!(snapshot.messages[0].text, "after compact");
        // Cold summaries live inline as synthetic messages; none tagged here.
        assert!(
            atomcode_kernel::message::cold_summaries_from_messages(&snapshot.messages).is_empty()
        );
    }

    #[test]
    fn prepare_catalog_session_resume_any_project_locates_session_in_another_bucket() {
        use atomcode_capabilities::session::{SessionManager, SessionMeta};
        use atomcode_kernel::message::SessionSnapshot;

        let root = tempfile::tempdir().unwrap();
        let proj1_dir = root.path().join("proj1");
        let proj2_dir = root.path().join("proj2");
        std::fs::create_dir_all(&proj1_dir).unwrap();
        std::fs::create_dir_all(&proj2_dir).unwrap();

        let bucket1 = SessionManager::project_hash(&proj1_dir);
        let bucket2 = SessionManager::project_hash(&proj2_dir);

        let mgr2 = SessionManager::with_root(root.path().join(&bucket2));
        let lease2 = mgr2.acquire_lease("session-in-proj2").unwrap();
        let mut meta2 = SessionMeta::new("session-in-proj2", proj2_dir.to_string_lossy(), 1000);
        meta2.owner = atomcode_capabilities::session::StorageOwner::Native;
        let snap2 = SessionSnapshot::new(vec![]);
        let pres2 = atomcode_capabilities::session::PresentationFile::default();
        mgr2.commit_native_import(&lease2, Some(&snap2), Some(&pres2), &meta2)
            .unwrap();
        drop(lease2);

        // Searching explicitly in proj1 bucket fails
        let res1 = crate::legacy_convert::prepare_catalog_session_resume_in_project_root(
            root.path(),
            &bucket1,
            "session-in-proj2",
            false,
        )
        .unwrap();
        assert!(res1.is_none());

        // Searching across any project finds it in proj2 bucket
        let res2 = crate::legacy_convert::prepare_catalog_session_resume_any_project_in_root(
            root.path(),
            "session-in-proj2",
        )
        .unwrap();
        assert!(res2.is_some());
        let prepared = res2.unwrap();
        assert_eq!(prepared.project_bucket, bucket2);
        assert_eq!(prepared.view.meta.working_dir, proj2_dir.to_string_lossy());
    }
}
