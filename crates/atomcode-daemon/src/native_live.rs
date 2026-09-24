use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use atomcode_coding::{
    CodingRuntime, CodingRuntimeConfig, DriverCommand, RuntimeMode, RuntimePhase, UserInput,
};
use atomcode_kernel::message::SessionSnapshot;
use atomcode_telemetry::Telemetry;
use tokio::sync::Mutex;

use crate::live_hub::{HubError, LiveBinding, LiveJoin, LiveRuntimeControl, LiveViewHub};

static HUB: OnceLock<Arc<LiveViewHub>> = OnceLock::new();
static EMBEDDED_BINDING: StdMutex<Option<LiveBinding>> = StdMutex::new(None);
static HEADLESS: OnceLock<Mutex<Option<HeadlessRuntime>>> = OnceLock::new();
static REMOTE_COMMAND: StdMutex<Option<tokio::sync::mpsc::UnboundedSender<String>>> =
    StdMutex::new(None);

struct HeadlessRuntime {
    binding: LiveBinding,
    handle: atomcode_coding::CodingRuntimeHandle,
}

fn hub() -> &'static Arc<LiveViewHub> {
    HUB.get_or_init(|| Arc::new(LiveViewHub::new()))
}

fn headless() -> &'static Mutex<Option<HeadlessRuntime>> {
    HEADLESS.get_or_init(|| Mutex::new(None))
}

pub fn register_embedded_runtime(
    session_id: String,
    working_dir: PathBuf,
    provider: String,
    provider_fingerprint: String,
    snapshot: SessionSnapshot,
    control: Arc<dyn LiveRuntimeControl>,
) -> Result<LiveBinding, HubError> {
    let headless_owner = headless()
        .try_lock()
        .map_err(|_| HubError::RuntimeUnavailable)?;
    if headless_owner.is_some() {
        return Err(HubError::RuntimeUnavailable);
    }
    let binding = hub().bind_with_provider(
        session_id,
        working_dir,
        provider,
        provider_fingerprint,
        snapshot,
        control,
    )?;
    *EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(binding.clone());
    // Seed the daemon's project state to the shared TUI's working dir so the webui footer
    // + session list match it. A no-op if the server hasn't started yet (DAEMON_PROJECT is
    // None) — that case is covered by `init_project_state` reading the embedded binding at
    // startup; this call handles an ALREADY-running (persistent) daemon, where the embed
    // happens after init. `/cd` keeps it current afterward via the same `live_set_working_dir`.
    crate::live_set_working_dir(binding.working_dir.clone());
    Ok(binding)
}

pub fn embedded_binding() -> Option<LiveBinding> {
    EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

pub fn unregister_embedded_runtime(binding: &LiveBinding) -> Result<(), HubError> {
    hub().unbind(binding)?;
    let mut embedded = EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if embedded
        .as_ref()
        .is_some_and(|current| current.id == binding.id)
    {
        *embedded = None;
        *REMOTE_COMMAND
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
    Ok(())
}

pub fn register_remote_command_sink() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    *REMOTE_COMMAND
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(sender);
    receiver
}

pub fn send_remote_command(command: String) -> bool {
    REMOTE_COMMAND
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .is_some_and(|sender| sender.send(command).is_ok())
}

pub fn publish(
    binding: &LiveBinding,
    event: atomcode_coding::SequencedRuntimeEvent,
) -> Result<(), HubError> {
    hub().publish(binding, event)
}

pub fn publish_unsequenced(
    binding: &LiveBinding,
    event: atomcode_coding::CodingRuntimeEvent,
) -> Result<(), HubError> {
    hub().publish_unsequenced(binding, event)
}

pub fn seed_goal_progress(
    binding: &LiveBinding,
    progress: atomcode_coding::GoalProgress,
) -> Result<(), HubError> {
    hub().seed_goal_progress(binding, progress)
}

pub fn join() -> Result<LiveJoin, HubError> {
    hub().join()
}

pub fn join_for_provider(expected_session_id: Option<&str>) -> Result<LiveJoin, HubError> {
    hub().join_for_provider(expected_session_id)
}

pub fn binding() -> Result<LiveBinding, HubError> {
    hub().binding()
}

pub fn submit(input: UserInput) -> Result<(), HubError> {
    hub().submit(input)
}

pub async fn submit_confirmed(
    input: UserInput,
) -> Result<atomcode_coding::SubmitReceipt, HubError> {
    hub().submit_confirmed(input).await
}

/// Submit `runtime_input` to the model while echoing `echo_input` to the live view
/// (see [`crate::live_hub::LiveViewHub::submit_confirmed_with_echo`]). Used by the
/// webui image path so the VL caption feeds the model but the user's original
/// message + image is what displays.
pub async fn submit_confirmed_with_echo(
    runtime_input: UserInput,
    echo_input: UserInput,
    client_input_id: Option<String>,
) -> Result<atomcode_coding::SubmitReceipt, HubError> {
    hub()
        .submit_confirmed_with_echo(runtime_input, echo_input, client_input_id)
        .await
}

pub fn accept_local_input(input: UserInput) -> Result<(), HubError> {
    hub().accept_local_input(input)
}

pub fn respond(
    id: atomcode_kernel::event::RequestId,
    value: serde_json::Value,
) -> Result<(), HubError> {
    hub().respond(id, value)
}

pub fn respond_pending_kind(kind: &str, value: serde_json::Value) -> Result<u64, HubError> {
    hub().respond_pending_kind(kind, value)
}

pub async fn respond_confirmed(
    id: atomcode_kernel::event::RequestId,
    value: serde_json::Value,
) -> Result<(), HubError> {
    hub().respond_confirmed(id, value).await
}

pub async fn resolve_policy_intervention(
    intervention_id: u64,
    action: atomcode_kernel::event::PolicyRecoveryAction,
) -> Result<(), HubError> {
    hub()
        .resolve_policy_intervention(intervention_id, action)
        .await
}

pub async fn respond_pending_kind_confirmed(
    kind: &str,
    value: serde_json::Value,
) -> Result<u64, HubError> {
    hub().respond_pending_kind_confirmed(kind, value).await
}

pub fn cancel() -> Result<(), HubError> {
    hub().cancel()
}

pub async fn cancel_confirmed() -> Result<(), HubError> {
    hub().cancel_confirmed().await
}

/// Force-tear-down the daemon's headless live runtime regardless of its phase or
/// turn state, and unbind — so a wedged/orphaned runtime can be replaced.
///
/// The explicit recovery for feedback B12. `ensure_headless_runtime` refuses to
/// replace a runtime whose phase is `InTurn`/`WaitingApproval`/`Reconfiguring`,
/// while `cancel_confirmed` refuses when the hub's `turn_active` is false — so if
/// those two ever disagree (a stalled reconfigure, an orphaned approval whose
/// consumer disconnected), NEITHER the rebind nor the cancel converges it and
/// every `GET /live?session_id=` 404s forever. This is the escape hatch a client
/// calls after that refusal: kill the current turn and unbind, unconditionally.
///
/// `Ok(true)` when something was released, `Ok(false)` when nothing was bound.
/// Scoped to the daemon-owned (headless) runtime; an **embedded** runtime (the
/// in-process TUI's) is refused — it is the TUI's to own, not the daemon's to kill.
pub async fn force_release() -> Result<bool, String> {
    if embedded_binding().is_some() {
        return Err("live runtime is owned by the in-process TUI; not force-releasing it".into());
    }
    let mut owner = headless().lock().await;
    let Some(old) = owner.take() else {
        return Ok(false);
    };
    // Best-effort shutdown: a handle whose task already died still needs the hub
    // reset, so a shutdown error must not stop it. Then `force_unbind` (NOT
    // `unbind`) — the wedge is `turn_active == true`, which ordinary `unbind`
    // refuses; force_unbind clears it too, so the next bind actually succeeds.
    // The runtime handle is gone by now, so there is no live turn to protect.
    let _ = old.handle.shutdown().await;
    hub().force_unbind();
    Ok(true)
}

pub fn dispatch(command: DriverCommand) -> Result<(), HubError> {
    hub().dispatch(command)
}

pub async fn set_mode(mode: RuntimeMode) -> Result<(), HubError> {
    hub().set_mode(mode).await
}

pub async fn reload_provider(
    expected: &LiveBinding,
    next: atomcode_coding::CodingAgentConfig,
    provider_fingerprint: String,
) -> Result<atomcode_coding::RuntimeGeneration, HubError> {
    hub()
        .reload_provider(expected, next, provider_fingerprint)
        .await
}

pub fn provider_fingerprint(
    config: &atomcode_config::config::Config,
    provider_name: &str,
) -> Result<String, String> {
    use sha2::{Digest, Sha256};

    if !config.selection_exists(provider_name) {
        return Err(format!("provider {provider_name:?} not found"));
    }
    let mut normalized = config.clone();
    normalized.default_provider = provider_name.to_string();
    // Serialize through Value so map keys are canonicalized before hashing;
    // Config contains HashMaps whose iteration order differs across processes.
    let canonical = serde_json::to_value(&normalized)
        .map_err(|error| format!("serialize provider configuration failed: {error}"))?;
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|error| format!("serialize provider configuration failed: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub async fn resume_session(
    session_id: String,
) -> Result<atomcode_coding::SessionChanged, HubError> {
    let binding = hub().binding()?;
    if binding.session_id == session_id {
        return Ok(atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(binding.generation),
            session_id: Some(binding.session_id),
            working_dir: binding.working_dir,
        });
    }
    let project_bucket =
        atomcode_capabilities::session::SessionManager::project_hash(&binding.working_dir);
    let prepared = match crate::legacy_convert::prepare_catalog_session_resume_in_project(
        &project_bucket,
        &session_id,
    ) {
        Ok(Some(prepared)) => prepared,
        _ => crate::legacy_convert::prepare_catalog_session_resume_any_project(&session_id)
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))?
            .ok_or_else(|| {
                HubError::RuntimeRejected(format!("session {session_id:?} not found in catalog"))
            })?,
    };
    let target_dir = PathBuf::from(&prepared.view.meta.working_dir);
    hub()
        .resume_session_with_lease(session_id, target_dir, prepared.lease)
        .await
}

/// Move the bound runtime to a fresh staged session. This is the only safe way
/// for the daemon to release the current idle session's lease before deleting
/// that session from disk.
pub async fn fresh_session(
    expected: &LiveBinding,
) -> Result<crate::live_hub::FreshSessionOutcome, HubError> {
    hub().fresh_session(expected).await
}

pub async fn change_directory(
    working_dir: PathBuf,
) -> Result<atomcode_coding::SessionChanged, HubError> {
    hub().change_directory(working_dir).await
}

pub async fn reload_capabilities() -> Result<atomcode_coding::SessionChanged, HubError> {
    hub().reload_capabilities().await
}

pub fn publish_command_output(text: String) -> Result<(), HubError> {
    hub().publish_command_output(text)
}

pub fn replace_snapshot(
    binding: &LiveBinding,
    session_id: String,
    working_dir: PathBuf,
    snapshot: SessionSnapshot,
) -> Result<LiveBinding, HubError> {
    let next = hub().replace_snapshot(binding, session_id, working_dir, snapshot)?;
    let mut embedded = EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if embedded
        .as_ref()
        .is_some_and(|current| current.id == binding.id)
    {
        *embedded = Some(next.clone());
    }
    Ok(next)
}

pub fn commit_runtime_snapshot(
    binding: &LiveBinding,
    session_id: String,
    working_dir: PathBuf,
    snapshot: SessionSnapshot,
) -> Result<LiveBinding, HubError> {
    let next = hub().commit_runtime_snapshot(binding, session_id, working_dir, snapshot)?;
    let mut embedded = EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if embedded
        .as_ref()
        .is_some_and(|current| current.id == binding.id)
    {
        *embedded = Some(next.clone());
    }
    Ok(next)
}

fn load_snapshot(working_dir: &Path, session_id: &str) -> Result<SessionSnapshot, String> {
    let bucket = atomcode_capabilities::session::SessionManager::project_hash(working_dir);
    crate::legacy_convert::load_catalog_session_view_in_project(&bucket, session_id)
        .map_err(|error| error.to_string())?
        .map(|session| session.snapshot)
        .ok_or_else(|| format!("session {session_id:?} not found"))
}

async fn bind_after_mcp_ready<T, E>(
    readiness: impl std::future::Future<Output = Result<(), E>>,
    bind: impl FnOnce() -> Result<T, String>,
) -> Result<T, String>
where
    E: std::fmt::Debug,
{
    readiness
        .await
        .map_err(|error| format!("MCP readiness wait failed: {error:?}"))?;
    bind()
}

/// Why a live runtime could not be joined — and, when another runtime is in
/// the way, which one and in what phase.
///
/// Serialized as the bare message, so `{"error": …}` is the same string on the
/// wire it always was and a client that matches it keeps working; who is in
/// the way is carried separately ([`occupant`](Self::occupant)) for the caller
/// that wants to report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveJoinError {
    message: String,
    occupant: Option<LiveOccupant>,
}

/// The runtime that holds the live binding a request wanted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LiveOccupant {
    pub session_id: String,
    pub working_dir: String,
    pub phase: String,
}

impl LiveJoinError {
    pub fn occupant(&self) -> Option<&LiveOccupant> {
        self.occupant.as_ref()
    }
}

impl From<String> for LiveJoinError {
    fn from(message: String) -> Self {
        Self {
            message,
            occupant: None,
        }
    }
}

impl std::fmt::Display for LiveJoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl serde::Serialize for LiveJoinError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.message)
    }
}

/// The refusal for a rebind while `binding`'s runtime is busy (`phase`).
///
/// The opening words are the ones this refusal has always had — adapters match
/// on them — and after them, which session holds the binding and doing what,
/// since with every health check green that is the one thing an operator
/// cannot otherwise see.
fn occupied_by(binding: &LiveBinding, phase: RuntimePhase) -> LiveJoinError {
    let phase = format!("{phase:?}");
    LiveJoinError {
        message: format!(
            "cannot replace an active live runtime: session {} is {phase} — if it is \
             wedged/orphaned (its consumer disconnected mid-turn), POST /live/release to \
             force-release it, then retry",
            binding.session_id
        ),
        occupant: Some(LiveOccupant {
            session_id: binding.session_id.clone(),
            working_dir: binding.working_dir.display().to_string(),
            phase,
        }),
    }
}

pub async fn ensure_headless_runtime(
    working_dir: PathBuf,
    telemetry: Arc<Telemetry>,
    provider_name: String,
    mode: RuntimeMode,
    requested_session_id: Option<String>,
) -> Result<LiveJoin, LiveJoinError> {
    if let Some(binding) = embedded_binding() {
        if requested_session_id
            .as_deref()
            .is_some_and(|requested| requested != binding.session_id)
        {
            return Err(format!(
                "embedded runtime is bound to session {:?}, requested {:?}",
                binding.session_id,
                requested_session_id.as_deref().unwrap_or_default()
            )
            .into());
        }
        return join().map_err(|error| format!("live hub join failed: {error:?}").into());
    }

    let mut owner = headless().lock().await;
    let can_reuse = owner.is_some()
        && join().is_ok_and(|current| {
            current.binding.working_dir == working_dir
                && requested_session_id
                    .as_deref()
                    .is_none_or(|requested| requested == current.binding.session_id)
        });
    if can_reuse {
        return join().map_err(|error| format!("live hub join failed: {error:?}").into());
    }

    if let Some(old) = owner.take() {
        let phase = old.handle.status().phase;
        if matches!(
            phase,
            RuntimePhase::InTurn | RuntimePhase::WaitingApproval | RuntimePhase::Reconfiguring
        ) {
            let refused = occupied_by(&old.binding, phase);
            *owner = Some(old);
            return Err(refused);
        }
        old.handle
            .shutdown()
            .await
            .map_err(|_| "failed to stop previous live runtime".to_string())?;
        // Shut down, so there is no turn left to protect: a `turn_active` the
        // hub still holds only means the terminal event never reached it, and
        // the plain `unbind` would refuse — leaving this dead runtime bound and
        // the bind below rejected. Someone else's binding is not ours to clear.
        match hub().unbind_retired(&old.binding) {
            Ok(()) | Err(HubError::Unbound) | Err(HubError::StaleBinding) => {}
            Err(error) => {
                return Err(format!("failed to unbind previous live runtime: {error:?}").into())
            }
        }
    }

    let config =
        atomcode_config::config::Config::load(&atomcode_config::config::Config::default_path())
            .map_err(|error| error.to_string())?;
    if !config.selection_exists(&provider_name) {
        return Err(format!("provider {provider_name:?} not found").into());
    }
    let provider_fingerprint = provider_fingerprint(&config, &provider_name)?;
    let runtime_config: CodingRuntimeConfig =
        crate::live_api::live_runtime_config(&config, &provider_name, &working_dir, telemetry);
    let (session_mode, initial_snapshot) = match requested_session_id {
        Some(id) => {
            let snapshot = load_snapshot(&working_dir, &id)?;
            (
                atomcode_coding::SessionMode::ExternalSnapshot {
                    id,
                    snapshot: snapshot.clone(),
                },
                snapshot,
            )
        }
        None => (
            atomcode_coding::SessionMode::Fresh,
            SessionSnapshot::new(Vec::new()),
        ),
    };
    let (runtime, _) = crate::start_native_runtime_with_session(runtime_config, session_mode)
        .await
        .map_err(|error| error.to_string())?;
    let CodingRuntime {
        handle,
        mut events,
        task,
        session,
        ..
    } = runtime;
    handle
        .set_mode(mode)
        .await
        .map_err(|error| format!("failed to set live mode: {error}"))?;

    // Wait for initial MCP tools to be published to the mounted kernel catalog
    // before the first turn. Without this, a headless
    // runtime created by `atomcode.exe webui` (which has no pre-existing
    // CodingRuntime from the TUI) would start its first turn before background
    // MCP connections complete, making MCP tools invisible to the agent even
    // though `/mcp/status` shows them as connected.
    // Timeout prevents a stalled MCP server from blocking the first message.
    let session_id = session
        .map(|session| session.id)
        .ok_or_else(|| "live runtime started without a persistent session".to_string())?;
    let binding = bind_after_mcp_ready(
        handle.wait_mcp_ready(atomcode_capabilities::mcp::CONNECT_TIMEOUT),
        || {
            hub()
                .bind_with_provider(
                    session_id.clone(),
                    working_dir.clone(),
                    provider_name,
                    provider_fingerprint,
                    initial_snapshot,
                    Arc::new(handle.clone()),
                )
                .map_err(|error| format!("live hub bind failed: {error:?}"))
        },
    )
    .await?;
    let event_binding = binding.clone();
    let event_handle = handle.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let session_change = match &event.event {
                atomcode_coding::CodingRuntimeEvent::SessionChanged(changed) => {
                    Some((changed.session_id.clone(), changed.working_dir.clone()))
                }
                _ => None,
            };
            match hub().publish(&event_binding, event) {
                Ok(()) => {}
                Err(HubError::StaleEvent) => {
                    tracing::warn!("discarded stale live runtime event");
                    continue;
                }
                Err(error) => {
                    tracing::warn!("stopping live event forwarding: {error:?}");
                    break;
                }
            }
            if let Some((Some(session_id), working_dir)) = session_change {
                match event_handle.snapshot().await {
                    Ok(snapshot) => {
                        if let Err(error) = hub().commit_runtime_snapshot(
                            &event_binding,
                            session_id,
                            working_dir,
                            snapshot.as_ref().clone(),
                        ) {
                            tracing::warn!("live session snapshot commit failed: {error:?}");
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!("live runtime session snapshot unavailable: {error}");
                    }
                }
            }
        }
        let _ = task.await;
    });
    *owner = Some(HeadlessRuntime { binding, handle });
    drop(owner);
    join().map_err(|error| format!("live hub join failed: {error:?}").into())
}

#[cfg(test)]
mod tests {
    use super::bind_after_mcp_ready;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Refused because another runtime is busy: the refusal says **which one
    /// and doing what**. "cannot replace an active live runtime" alone left an
    /// operator with every health check green and no way to see that session X
    /// was parked on an approval nobody could answer.
    #[test]
    fn a_refused_rebind_names_the_runtime_in_the_way() {
        let binding = crate::live_hub::LiveBinding {
            id: 3,
            generation: 1,
            session_id: "session-held".into(),
            working_dir: std::path::PathBuf::from("/work/proj"),
            provider: "p".into(),
            provider_fingerprint: "f".into(),
        };
        let refused = super::occupied_by(&binding, atomcode_coding::RuntimePhase::WaitingApproval);

        let said = refused.to_string();
        // What clients already match on stays where it was.
        assert!(
            said.starts_with("cannot replace an active live runtime"),
            "{said}"
        );
        assert!(
            said.contains("session-held") && said.contains("WaitingApproval"),
            "{said}"
        );
        assert!(
            said.contains("POST /live/release"),
            "the way out is still named: {said}"
        );

        // `error` stays a string on the wire — an adapter that matches it keeps
        // working — and who is in the way rides beside it.
        assert_eq!(
            serde_json::to_value(&refused).unwrap(),
            serde_json::Value::String(said)
        );
        assert_eq!(
            serde_json::to_value(refused.occupant()).unwrap(),
            serde_json::json!({
                "session_id": "session-held",
                "working_dir": "/work/proj",
                "phase": "WaitingApproval",
            })
        );

        // Any other failure has nobody in the way.
        let other = super::LiveJoinError::from("provider \"x\" not found".to_string());
        assert!(other.occupant().is_none());
        assert_eq!(
            serde_json::to_value(&other).unwrap(),
            serde_json::json!("provider \"x\" not found")
        );
    }

    #[tokio::test]
    async fn headless_bind_waits_for_mcp_catalog_readiness() {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (waiting_tx, waiting_rx) = tokio::sync::oneshot::channel::<()>();
        let bound = Arc::new(AtomicBool::new(false));
        let bound_in_task = Arc::clone(&bound);

        let bind_task = tokio::spawn(async move {
            bind_after_mcp_ready(
                async move {
                    waiting_tx.send(()).unwrap();
                    ready_rx.await.expect("readiness sender must stay alive");
                    Ok::<(), &'static str>(())
                },
                || {
                    bound_in_task.store(true, Ordering::Release);
                    Ok(())
                },
            )
            .await
        });

        waiting_rx.await.unwrap();
        assert!(
            !bound.load(Ordering::Acquire),
            "the live hub must remain unbound while MCP tools are unpublished"
        );

        ready_tx.send(()).unwrap();
        bind_task.await.unwrap().unwrap();
        assert!(
            bound.load(Ordering::Acquire),
            "the live hub should bind after MCP tools reach the catalog"
        );
    }
}
