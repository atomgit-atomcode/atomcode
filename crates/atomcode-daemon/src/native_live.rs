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
    snapshot: SessionSnapshot,
    control: Arc<dyn LiveRuntimeControl>,
) -> Result<LiveBinding, HubError> {
    let binding = hub().bind(session_id, working_dir, snapshot, control)?;
    *EMBEDDED_BINDING
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(binding.clone());
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

pub fn join() -> Result<LiveJoin, HubError> {
    hub().join()
}

pub fn submit(input: UserInput) -> Result<(), HubError> {
    hub().submit(input)
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

pub fn cancel() -> Result<(), HubError> {
    hub().cancel()
}

pub fn dispatch(command: DriverCommand) -> Result<(), HubError> {
    hub().dispatch(command)
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
    hub().replace_snapshot(binding, session_id, working_dir, snapshot)
}

pub fn commit_runtime_snapshot(
    binding: &LiveBinding,
    session_id: String,
    working_dir: PathBuf,
    snapshot: SessionSnapshot,
) -> Result<LiveBinding, HubError> {
    hub().commit_runtime_snapshot(binding, session_id, working_dir, snapshot)
}

fn load_snapshot(working_dir: &Path, session_id: &str) -> Result<SessionSnapshot, String> {
    let bucket = atomcode_capabilities::session::SessionManager::project_hash(working_dir);
    crate::legacy_convert::load_catalog_session_view_in_project(&bucket, session_id)
        .map_err(|error| error.to_string())?
        .map(|session| session.snapshot)
        .ok_or_else(|| format!("session {session_id:?} not found"))
}

pub async fn ensure_headless_runtime(
    working_dir: PathBuf,
    telemetry: Arc<Telemetry>,
    provider_name: String,
    mode: RuntimeMode,
    requested_session_id: Option<String>,
) -> Result<LiveJoin, String> {
    if let Some(binding) = embedded_binding() {
        if requested_session_id
            .as_deref()
            .is_some_and(|requested| requested != binding.session_id)
        {
            return Err(format!(
                "embedded runtime is bound to session {:?}, requested {:?}",
                binding.session_id,
                requested_session_id.as_deref().unwrap_or_default()
            ));
        }
        return join().map_err(|error| format!("live hub join failed: {error:?}"));
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
        return join().map_err(|error| format!("live hub join failed: {error:?}"));
    }

    if let Some(old) = owner.take() {
        if matches!(
            old.handle.status().phase,
            RuntimePhase::InTurn | RuntimePhase::WaitingApproval | RuntimePhase::Reconfiguring
        ) {
            *owner = Some(old);
            return Err("cannot replace an active live runtime".into());
        }
        old.handle
            .shutdown()
            .await
            .map_err(|_| "failed to stop previous live runtime".to_string())?;
        let _ = hub().unbind(&old.binding);
    }

    let config =
        atomcode_config::config::Config::load(&atomcode_config::config::Config::default_path())
            .map_err(|error| error.to_string())?;
    if !config.providers.contains_key(&provider_name) {
        return Err(format!("provider {provider_name:?} not found"));
    }
    let runtime_config: CodingRuntimeConfig =
        crate::live_api::chat_runtime_config(&config, &provider_name, &working_dir, telemetry);
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
    let session_id = session
        .map(|session| session.id)
        .ok_or_else(|| "live runtime started without a persistent session".to_string())?;
    let binding = hub()
        .bind(
            session_id.clone(),
            working_dir.clone(),
            initial_snapshot,
            Arc::new(handle.clone()),
        )
        .map_err(|error| format!("live hub bind failed: {error:?}"))?;
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
    join().map_err(|error| format!("live hub join failed: {error:?}"))
}
