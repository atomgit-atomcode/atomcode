use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use atomcode_coding::{
    CodingRuntimeEvent, CodingRuntimeHandle, DriverCommand, RuntimePhase, RuntimeStatus,
    RuntimeUnavailable, SequencedRuntimeEvent, SubmitReceipt, TurnCompletion, UserInput,
};
use atomcode_kernel::event::{AgentEvent, RequestId};
use atomcode_kernel::message::SessionSnapshot;
use tokio::sync::broadcast;

const BROADCAST_CAPACITY: usize = 1024;

pub trait LiveRuntimeControl: Send + Sync {
    fn status(&self) -> RuntimeStatus;
    fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable>;
    fn handle(&self) -> Option<CodingRuntimeHandle>;
}

impl LiveRuntimeControl for CodingRuntimeHandle {
    fn status(&self) -> RuntimeStatus {
        CodingRuntimeHandle::status(self)
    }

    fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable> {
        CodingRuntimeHandle::dispatch(self, command)
    }

    fn handle(&self) -> Option<CodingRuntimeHandle> {
        Some(self.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveBinding {
    pub id: u64,
    pub generation: u64,
    pub session_id: String,
    pub working_dir: PathBuf,
    pub provider: String,
    pub provider_fingerprint: String,
}

#[derive(Clone, Debug)]
pub enum LiveViewEvent {
    InputAccepted {
        input: UserInput,
        client_input_id: Option<String>,
    },
    /// Browser-facing correlation for a kernel steer acknowledgement. The raw
    /// runtime event is still published for embedded drivers such as the TUI;
    /// this additive projection keeps transport identity out of the kernel.
    Steered {
        count: usize,
        inputs: Vec<atomcode_kernel::event::SteeredInput>,
        client_input_ids: Vec<Option<String>>,
    },
    CommandOutput(String),
    /// A driver successfully delivered the response for a previously published
    /// runtime request. This is view coordination owned by the live hub, not a
    /// second runtime terminal: peers use it to dismiss the matching prompt.
    RequestResolved {
        request_id: RequestId,
        kind: String,
    },
    Runtime(CodingRuntimeEvent),
}

#[derive(Clone, Debug)]
pub struct LiveObservation {
    pub binding_id: u64,
    pub generation: u64,
    pub cursor: u64,
    pub event: LiveViewEvent,
}

pub struct LiveJoin {
    pub binding: LiveBinding,
    pub snapshot: Arc<SessionSnapshot>,
    /// Latest Goal controller state. Unlike turn replay, this must survive a
    /// completed turn so late `/live` subscribers can restore the Goal UI.
    pub goal_progress: Option<atomcode_coding::GoalProgress>,
    pub replay: Vec<LiveObservation>,
    pub receiver: broadcast::Receiver<LiveObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshSessionOutcome {
    pub changed: atomcode_coding::SessionChanged,
    /// The runtime transition is already committed when this is populated.
    /// Callers must not report the operation as wholly rejected or retry the
    /// transition against the replacement runtime.
    pub projection_error: Option<HubError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubError {
    Unbound,
    StaleBinding,
    StaleEvent,
    RuntimeUnavailable,
    RuntimeGenerationChanged { expected: u64, actual: u64 },
    UnknownRequest(RequestId),
    ActiveTurn,
    NoActiveTurn,
    SnapshotUnavailable(String),
    RuntimeRejected(String),
}

struct BoundRuntime {
    identity: LiveBinding,
    control: Arc<dyn LiveRuntimeControl>,
}

#[derive(Default)]
struct HubState {
    next_binding_id: u64,
    next_cursor: u64,
    binding: Option<BoundRuntime>,
    snapshot: Option<Arc<SessionSnapshot>>,
    snapshot_error: Option<String>,
    replay: Vec<LiveObservation>,
    goal_progress: Option<atomcode_coding::GoalProgress>,
    pending_requests: HashMap<RequestId, String>,
    turn_active: bool,
    last_runtime_sequence: Option<u64>,
    pending_web_steers: VecDeque<PendingWebSteer>,
}

#[derive(Clone)]
struct PendingWebSteer {
    runtime_input: UserInput,
    client_input_id: String,
}

fn remove_pending_web_steer_locked(state: &mut HubState, client_input_id: Option<&str>) {
    let Some(client_input_id) = client_input_id else {
        return;
    };
    if let Some(index) = state
        .pending_web_steers
        .iter()
        .position(|pending| pending.client_input_id == client_input_id)
    {
        state.pending_web_steers.remove(index);
    }
}

fn correlate_web_steers_locked(
    state: &mut HubState,
    inputs: &[atomcode_kernel::event::SteeredInput],
) -> Vec<Option<String>> {
    inputs
        .iter()
        .map(|input| {
            let matches_front = state.pending_web_steers.front().is_some_and(|pending| {
                pending.runtime_input.text == input.text
                    && pending.runtime_input.images == input.images
            });
            matches_front.then(|| {
                state
                    .pending_web_steers
                    .pop_front()
                    .expect("front was checked above")
                    .client_input_id
            })
        })
        .collect()
}

pub struct LiveViewHub {
    state: Mutex<HubState>,
    events: broadcast::Sender<LiveObservation>,
}

impl Default for LiveViewHub {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveViewHub {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            state: Mutex::new(HubState::default()),
            events,
        }
    }

    #[cfg(test)]
    pub fn bind(
        &self,
        session_id: impl Into<String>,
        working_dir: PathBuf,
        snapshot: SessionSnapshot,
        control: Arc<dyn LiveRuntimeControl>,
    ) -> Result<LiveBinding, HubError> {
        self.bind_with_provider(
            session_id,
            working_dir,
            String::new(),
            String::new(),
            snapshot,
            control,
        )
    }

    pub fn bind_with_provider(
        &self,
        session_id: impl Into<String>,
        working_dir: PathBuf,
        provider: impl Into<String>,
        provider_fingerprint: impl Into<String>,
        snapshot: SessionSnapshot,
        control: Arc<dyn LiveRuntimeControl>,
    ) -> Result<LiveBinding, HubError> {
        let status = control.status();
        match status.phase {
            RuntimePhase::InTurn | RuntimePhase::WaitingApproval | RuntimePhase::Reconfiguring => {
                return Err(HubError::ActiveTurn)
            }
            RuntimePhase::ShuttingDown | RuntimePhase::Stopped | RuntimePhase::Failed => {
                return Err(HubError::RuntimeUnavailable);
            }
            RuntimePhase::Ready | RuntimePhase::AwaitingProvider => {}
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.turn_active {
            return Err(HubError::ActiveTurn);
        }
        state.next_binding_id += 1;
        let identity = LiveBinding {
            id: state.next_binding_id,
            generation: status.generation,
            session_id: session_id.into(),
            working_dir,
            provider: provider.into(),
            provider_fingerprint: provider_fingerprint.into(),
        };
        state.binding = Some(BoundRuntime {
            identity: identity.clone(),
            control,
        });
        state.snapshot = Some(Arc::new(snapshot));
        state.snapshot_error = None;
        state.replay.clear();
        state.goal_progress = None;
        state.pending_requests.clear();
        state.pending_web_steers.clear();
        state.turn_active = false;
        state.last_runtime_sequence = None;
        Ok(identity)
    }

    pub fn join(&self) -> Result<LiveJoin, HubError> {
        self.join_for_provider(None)
    }

    pub fn join_for_provider(
        &self,
        expected_session_id: Option<&str>,
    ) -> Result<LiveJoin, HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let binding = state
            .binding
            .as_ref()
            .ok_or(HubError::Unbound)?
            .identity
            .clone();
        if expected_session_id.is_some_and(|expected| expected != binding.session_id) {
            return Err(HubError::StaleBinding);
        }
        if let Some(error) = &state.snapshot_error {
            return Err(HubError::SnapshotUnavailable(error.clone()));
        }
        let snapshot = state.snapshot.clone().ok_or(HubError::Unbound)?;
        let receiver = self.events.subscribe();
        Ok(LiveJoin {
            binding,
            snapshot,
            goal_progress: state.goal_progress.clone(),
            replay: state.replay.clone(),
            receiver,
        })
    }

    pub fn binding(&self) -> Result<LiveBinding, HubError> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .binding
            .as_ref()
            .map(|bound| bound.identity.clone())
            .ok_or(HubError::Unbound)
    }

    pub fn unbind(&self, binding: &LiveBinding) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current_binding_id = state
            .binding
            .as_ref()
            .map(|current| current.identity.id)
            .ok_or(HubError::Unbound)?;
        if current_binding_id != binding.id {
            return Err(HubError::StaleBinding);
        }
        if state.turn_active {
            return Err(HubError::ActiveTurn);
        }
        state.binding = None;
        state.snapshot = None;
        state.snapshot_error = None;
        state.replay.clear();
        state.goal_progress = None;
        state.pending_requests.clear();
        state.pending_web_steers.clear();
        state.last_runtime_sequence = None;
        Ok(())
    }

    pub fn replace_snapshot(
        &self,
        binding: &LiveBinding,
        session_id: String,
        working_dir: PathBuf,
        snapshot: SessionSnapshot,
    ) -> Result<LiveBinding, HubError> {
        self.replace_snapshot_inner(binding, session_id, working_dir, snapshot, true)
    }

    pub fn commit_runtime_snapshot(
        &self,
        binding: &LiveBinding,
        session_id: String,
        working_dir: PathBuf,
        snapshot: SessionSnapshot,
    ) -> Result<LiveBinding, HubError> {
        self.replace_snapshot_inner(binding, session_id, working_dir, snapshot, false)
    }

    fn replace_snapshot_inner(
        &self,
        binding: &LiveBinding,
        session_id: String,
        working_dir: PathBuf,
        snapshot: SessionSnapshot,
        announce: bool,
    ) -> Result<LiveBinding, HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_mut().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id {
            return Err(HubError::StaleBinding);
        }
        current.identity.generation = current.control.status().generation;
        current.identity.session_id = session_id;
        current.identity.working_dir = working_dir;
        let identity = current.identity.clone();
        state.snapshot = Some(Arc::new(snapshot));
        state.snapshot_error = None;
        state.replay.clear();
        state.goal_progress = None;
        state.pending_requests.clear();
        state.pending_web_steers.clear();
        state.turn_active = false;
        state.last_runtime_sequence = None;
        if announce {
            self.publish_view_locked(
                &mut state,
                LiveViewEvent::Runtime(CodingRuntimeEvent::SessionChanged(
                    atomcode_coding::SessionChanged {
                        generation: atomcode_coding::RuntimeGeneration(identity.generation),
                        session_id: Some(identity.session_id.clone()),
                        working_dir: identity.working_dir.clone(),
                    },
                )),
                false,
            );
        }
        Ok(identity)
    }

    pub fn submit(&self, input: UserInput) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Self::dispatch_locked(&state, DriverCommand::Submit(input.clone()))?;
        if !state.turn_active {
            state.replay.clear();
            state.pending_requests.clear();
            state.pending_web_steers.clear();
        }
        state.turn_active = true;
        self.publish_view_locked(
            &mut state,
            LiveViewEvent::InputAccepted {
                input,
                client_input_id: None,
            },
            true,
        );
        Ok(())
    }

    pub async fn submit_confirmed(&self, input: UserInput) -> Result<SubmitReceipt, HubError> {
        let echo = input.clone();
        self.submit_confirmed_with_echo(input, echo, None).await
    }

    fn remove_pending_web_steer(&self, client_input_id: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        remove_pending_web_steer_locked(&mut state, client_input_id);
    }

    /// Like [`Self::submit_confirmed`], but the view echo — what every subscribed
    /// tab (and a synchronized TUI) DISPLAYS, and what late joiners replay — is
    /// `echo_input`, DISTINCT from the `runtime_input` fed to the model.
    ///
    /// The webui image path needs this: it submits the VL-PREPROCESSED caption as
    /// `runtime_input` (a text-only model 400s on the raw image bytes) while
    /// echoing the user's ORIGINAL text + image, so the live view shows what the
    /// user actually typed instead of the machine-generated caption overwriting it.
    pub async fn submit_confirmed_with_echo(
        &self,
        runtime_input: UserInput,
        echo_input: UserInput,
        client_input_id: Option<String>,
    ) -> Result<SubmitReceipt, HubError> {
        let (binding, handle) = self.bound_handle()?;
        let correlation_registered = if let Some(client_input_id) = client_input_id.clone() {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
            if current.identity.id != binding.id {
                return Err(HubError::StaleBinding);
            }
            if state.turn_active {
                state.pending_web_steers.push_back(PendingWebSteer {
                    runtime_input: runtime_input.clone(),
                    client_input_id,
                });
                true
            } else {
                false
            }
        } else {
            false
        };
        let receipt = handle
            .submit(runtime_input)
            .await
            .map_err(|error| {
                if correlation_registered {
                    self.remove_pending_web_steer(client_input_id.as_deref());
                }
                HubError::RuntimeRejected(error.to_string())
            })?;
        let receipt_generation = match receipt {
            SubmitReceipt::Started { generation, .. }
            | SubmitReceipt::Steered { generation, .. } => generation,
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let (current_binding_id, current_generation) = state
            .binding
            .as_ref()
            .map(|current| (current.identity.id, current.identity.generation))
            .ok_or(HubError::Unbound)?;
        if current_binding_id != binding.id {
            if correlation_registered {
                remove_pending_web_steer_locked(&mut state, client_input_id.as_deref());
            }
            return Err(HubError::StaleBinding);
        }
        if current_generation != receipt_generation {
            if correlation_registered {
                remove_pending_web_steer_locked(&mut state, client_input_id.as_deref());
            }
            return Err(HubError::RuntimeGenerationChanged {
                expected: receipt_generation,
                actual: current_generation,
            });
        }
        if matches!(receipt, SubmitReceipt::Started { .. }) {
            state.replay.clear();
            state.pending_requests.clear();
            if correlation_registered {
                remove_pending_web_steer_locked(&mut state, client_input_id.as_deref());
            }
        }
        state.turn_active = true;
        self.publish_view_locked(
            &mut state,
            LiveViewEvent::InputAccepted {
                input: echo_input,
                client_input_id,
            },
            true,
        );
        Ok(receipt)
    }

    /// Record an input already accepted through the bound runtime's local driver.
    /// Embedded drivers should prefer [`Self::submit`]; this seam exists for
    /// inputs whose acceptance and UI echo are owned atomically by the local UI.
    pub fn accept_local_input(&self, input: UserInput) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let binding = state.binding.as_ref().ok_or(HubError::Unbound)?;
        let status = binding.control.status();
        if status.generation != binding.identity.generation {
            return Err(HubError::RuntimeGenerationChanged {
                expected: binding.identity.generation,
                actual: status.generation,
            });
        }
        if !state.turn_active {
            state.replay.clear();
            state.pending_web_steers.clear();
        }
        state.turn_active = true;
        state.pending_requests.clear();
        self.publish_view_locked(
            &mut state,
            LiveViewEvent::InputAccepted {
                input,
                client_input_id: None,
            },
            true,
        );
        Ok(())
    }

    pub fn dispatch(&self, command: DriverCommand) -> Result<(), HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Self::dispatch_locked(&state, command)
    }

    pub async fn set_mode(&self, mode: atomcode_coding::RuntimeMode) -> Result<(), HubError> {
        self.bound_handle()?
            .1
            .set_mode(mode)
            .await
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))
    }

    /// Whether the bound runtime is mid-turn, parked awaiting approval, or already
    /// reconfiguring. A provider reload in any of these states hard-kills the
    /// in-flight turn (via `AgentCommand::Shutdown`) and drops its context — the
    /// runtime respawns from the last on-disk snapshot, which predates the
    /// interrupted turn. The provider-switch entry points refuse so the user stops
    /// the turn first. Unbound → false (nothing to interrupt).
    ///
    /// This is a best-effort guard, not an atomic gate: the caller releases this
    /// lock before it re-acquires state to dispatch, and the runtime processes a
    /// concurrently-queued submit ahead of the reassemble, so a turn that starts
    /// in that narrow window can still be caught. Closing it fully needs
    /// runtime-level coordination (out of this fix's scope). The phase set mirrors
    /// [`Self::bind_with_provider`]'s active-turn set so both refuse identically.
    pub fn turn_in_progress(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.turn_active {
            return true;
        }
        let Some(bound) = state.binding.as_ref() else {
            return false;
        };
        matches!(
            bound.control.status().phase,
            RuntimePhase::InTurn | RuntimePhase::WaitingApproval | RuntimePhase::Reconfiguring
        )
    }

    pub async fn reload_provider(
        &self,
        expected: &LiveBinding,
        next: atomcode_coding::CodingAgentConfig,
        provider_fingerprint: String,
    ) -> Result<atomcode_coding::RuntimeGeneration, HubError> {
        // Refuse to swap the provider while a turn is running: reassemble would
        // hard-kill it and silently discard the interrupted turn's work.
        if self.turn_in_progress() {
            return Err(HubError::ActiveTurn);
        }
        let handle = self.bound_handle_for(expected)?;
        let provider = next.provider_name.clone();
        let generation = handle
            .reassemble_provider(next)
            .await
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))?;
        self.commit_reconfigure_generation(
            expected,
            generation,
            Some((provider, provider_fingerprint)),
        )?;
        Ok(generation)
    }

    pub async fn resume_session_with_lease(
        &self,
        session_id: String,
        working_dir: PathBuf,
        lease: atomcode_capabilities::session::SessionLease,
    ) -> Result<atomcode_coding::SessionChanged, HubError> {
        let (binding, handle) = self.bound_handle()?;
        let changed = handle
            .resume_session_with_lease(session_id, working_dir, lease)
            .await
            .map_err(map_session_transition_error)?;
        self.commit_changed_snapshot(&binding, &handle, &changed)
            .await?;
        Ok(changed)
    }

    /// Move an idle runtime to a fresh staged session. CodingRuntime owns the
    /// transition and releases the previous session lease before returning the
    /// terminal, which lets callers safely delete the old aggregate afterwards.
    pub async fn fresh_session(
        &self,
        expected: &LiveBinding,
    ) -> Result<FreshSessionOutcome, HubError> {
        let handle = self.bound_handle_for(expected)?;
        let changed = handle
            .fresh_session()
            .await
            .map_err(map_session_transition_error)?;
        let projection_error = self
            .commit_changed_snapshot(expected, &handle, &changed)
            .await
            .err();
        Ok(FreshSessionOutcome {
            changed,
            projection_error,
        })
    }

    pub async fn change_directory(
        &self,
        working_dir: PathBuf,
    ) -> Result<atomcode_coding::SessionChanged, HubError> {
        let (binding, handle) = self.bound_handle()?;
        let changed = handle
            .change_directory(working_dir)
            .await
            .map_err(map_session_transition_error)?;
        if session_change_is_noop(&binding, &changed) {
            return Ok(changed);
        }
        self.commit_changed_snapshot(&binding, &handle, &changed)
            .await?;
        Ok(changed)
    }

    pub async fn reload_capabilities(&self) -> Result<atomcode_coding::SessionChanged, HubError> {
        let (binding, handle) = self.bound_handle()?;
        let changed = handle
            .reload_capabilities_with_plugin_skills(Some(crate::gather_plugin_skill_dirs_for(
                &binding.working_dir,
            )))
            .await
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))?;
        self.commit_changed_snapshot(&binding, &handle, &changed)
            .await?;
        Ok(changed)
    }

    pub fn publish_command_output(&self, text: String) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.binding.is_none() {
            return Err(HubError::Unbound);
        }
        self.publish_view_locked(&mut state, LiveViewEvent::CommandOutput(text), false);
        Ok(())
    }

    pub fn respond(&self, id: RequestId, value: serde_json::Value) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.pending_requests.contains_key(&id) {
            return Err(HubError::UnknownRequest(id));
        }
        Self::dispatch_locked(&state, DriverCommand::Respond { id, value })?;
        self.resolve_request_locked(&mut state, id)?;
        Ok(())
    }

    pub async fn respond_confirmed(
        &self,
        id: RequestId,
        value: serde_json::Value,
    ) -> Result<(), HubError> {
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.pending_requests.contains_key(&id) {
                return Err(HubError::UnknownRequest(id));
            }
        }
        let (binding, handle) = self.bound_handle()?;
        handle
            .respond(id, value)
            .await
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id {
            return Err(HubError::StaleBinding);
        }
        // The accepted response can immediately resume and finish the turn. Its
        // terminal event clears `pending_requests` concurrently while this async
        // method is reacquiring the hub lock. That is already a successful
        // resolution, not an UnknownRequest failure; publish the peer terminal
        // only while the request is still present in this generation.
        if current.identity.generation == binding.generation
            && state.pending_requests.contains_key(&id)
        {
            self.resolve_request_locked(&mut state, id)?;
        }
        Ok(())
    }

    pub fn respond_pending_kind(
        &self,
        kind: &str,
        value: serde_json::Value,
    ) -> Result<RequestId, HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let id = state
            .pending_requests
            .iter()
            .find_map(|(id, pending_kind)| (pending_kind == kind).then_some(*id))
            .ok_or(HubError::UnknownRequest(0))?;
        Self::dispatch_locked(&state, DriverCommand::Respond { id, value })?;
        self.resolve_request_locked(&mut state, id)?;
        Ok(id)
    }

    pub async fn respond_pending_kind_confirmed(
        &self,
        kind: &str,
        value: serde_json::Value,
    ) -> Result<RequestId, HubError> {
        let id = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state
                .pending_requests
                .iter()
                .find_map(|(id, pending_kind)| (pending_kind == kind).then_some(*id))
                .ok_or(HubError::UnknownRequest(0))?
        };
        self.respond_confirmed(id, value).await?;
        Ok(id)
    }

    pub fn cancel(&self) -> Result<(), HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.turn_active {
            return Err(HubError::NoActiveTurn);
        }
        Self::dispatch_locked(&state, DriverCommand::Cancel)
    }

    pub async fn cancel_confirmed(&self) -> Result<(), HubError> {
        {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.turn_active {
                return Err(HubError::NoActiveTurn);
            }
        }
        self.bound_handle()?
            .1
            .cancel()
            .await
            .map_err(|error| HubError::RuntimeRejected(error.to_string()))
    }

    pub fn publish(
        &self,
        binding: &LiveBinding,
        envelope: SequencedRuntimeEvent,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id {
            return Err(HubError::StaleBinding);
        }
        if envelope.generation < current.identity.generation {
            return Err(HubError::StaleEvent);
        }
        if state
            .last_runtime_sequence
            .is_some_and(|last| envelope.sequence <= last)
        {
            return Err(HubError::StaleEvent);
        }
        if envelope.generation > current.identity.generation {
            let current = state.binding.as_mut().expect("binding checked above");
            current.identity.generation = envelope.generation;
            state.replay.clear();
            state.goal_progress = None;
            state.pending_requests.clear();
            state.pending_web_steers.clear();
            state.turn_active = false;
            state.snapshot_error = None;
        }
        state.last_runtime_sequence = Some(envelope.sequence);

        let event = envelope.event;
        if let CodingRuntimeEvent::GoalChanged(progress) = &event {
            state.goal_progress = (progress.phase != atomcode_coding::GoalPhase::Ended)
                .then(|| progress.clone());
        }
        let mapped_steer = match &event {
            CodingRuntimeEvent::Agent(AgentEvent::Steered { count, inputs }) => Some((
                *count,
                inputs.clone(),
                correlate_web_steers_locked(&mut state, inputs),
            )),
            _ => None,
        };
        let mut replay = state.turn_active;
        match &event {
            CodingRuntimeEvent::Agent(AgentEvent::TurnStarted) => {
                state.turn_active = true;
                replay = true;
            }
            CodingRuntimeEvent::Request(request) => {
                state.turn_active = true;
                state
                    .pending_requests
                    .insert(request.id, request.kind.clone());
                replay = true;
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { snapshot, .. }) => {
                state.snapshot = Some(snapshot.clone());
                state.snapshot_error = None;
                state.replay.clear();
                state.pending_requests.clear();
                state.pending_web_steers.clear();
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                error, ..
            }) => {
                state.snapshot_error = Some(error.message.clone());
                state.pending_requests.clear();
                state.pending_web_steers.clear();
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::RuntimeStopped(_) => {
                state.snapshot_error = Some("runtime stopped".into());
                state.pending_requests.clear();
                state.pending_web_steers.clear();
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::SessionChanged(changed) => {
                let current = state.binding.as_mut().expect("binding checked above");
                let identity_changed = changed
                    .session_id
                    .as_ref()
                    .is_some_and(|session_id| session_id != &current.identity.session_id)
                    || changed.working_dir != current.identity.working_dir;
                if let Some(session_id) = &changed.session_id {
                    current.identity.session_id = session_id.clone();
                }
                current.identity.working_dir = changed.working_dir.clone();
                if identity_changed {
                    // The identity changes before the owner can asynchronously fetch
                    // the replacement snapshot. Never pair it with the previous
                    // session's snapshot during that window.
                    state.snapshot_error = Some("session snapshot pending".into());
                    state.replay.clear();
                    state.goal_progress = None;
                    state.pending_requests.clear();
                    state.pending_web_steers.clear();
                    state.turn_active = false;
                }
            }
            CodingRuntimeEvent::WorkingDirectoryChanged(working_dir) => {
                let current = state.binding.as_mut().expect("binding checked above");
                current.identity.working_dir = working_dir.clone();
            }
            CodingRuntimeEvent::ProviderChanged { provider, .. } => {
                let current = state.binding.as_mut().expect("binding checked above");
                if current.identity.provider != *provider {
                    current.identity.provider_fingerprint.clear();
                }
                current.identity.provider = provider.clone();
            }
            _ => {}
        }
        self.publish_view_locked(&mut state, LiveViewEvent::Runtime(event), replay);
        if let Some((count, inputs, client_input_ids)) = mapped_steer {
            self.publish_view_locked(
                &mut state,
                LiveViewEvent::Steered {
                    count,
                    inputs,
                    client_input_ids,
                },
                replay,
            );
        }
        Ok(())
    }

    pub fn publish_unsequenced(
        &self,
        binding: &LiveBinding,
        event: CodingRuntimeEvent,
    ) -> Result<(), HubError> {
        let envelope = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
            if current.identity.id != binding.id {
                return Err(HubError::StaleBinding);
            }
            SequencedRuntimeEvent {
                generation: current.control.status().generation,
                sequence: state
                    .last_runtime_sequence
                    .map_or(0, |sequence| sequence.wrapping_add(1)),
                event,
            }
        };
        self.publish(binding, envelope)
    }

    /// Seed controller-only state when an already-running embedded TUI is
    /// attached. This is deliberately outside the runtime sequence stream:
    /// assigning sequence 0 here could make the runtime's first real event
    /// (also sequence 0) look stale and drop it.
    pub fn seed_goal_progress(
        &self,
        binding: &LiveBinding,
        progress: atomcode_coding::GoalProgress,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id {
            return Err(HubError::StaleBinding);
        }
        state.goal_progress = Some(progress.clone());
        self.publish_view_locked(
            &mut state,
            LiveViewEvent::Runtime(CodingRuntimeEvent::GoalChanged(progress)),
            false,
        );
        Ok(())
    }

    fn dispatch_locked(state: &HubState, command: DriverCommand) -> Result<(), HubError> {
        let binding = state.binding.as_ref().ok_or(HubError::Unbound)?;
        let status = binding.control.status();
        if status.generation != binding.identity.generation {
            return Err(HubError::RuntimeGenerationChanged {
                expected: binding.identity.generation,
                actual: status.generation,
            });
        }
        binding
            .control
            .dispatch(command)
            .map_err(|_| HubError::RuntimeUnavailable)
    }

    fn bound_handle(&self) -> Result<(LiveBinding, CodingRuntimeHandle), HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(error) = &state.snapshot_error {
            return Err(HubError::SnapshotUnavailable(error.clone()));
        }
        let binding = state.binding.as_ref().ok_or(HubError::Unbound)?;
        let status = binding.control.status();
        if status.generation != binding.identity.generation {
            return Err(HubError::RuntimeGenerationChanged {
                expected: binding.identity.generation,
                actual: status.generation,
            });
        }
        let handle = binding
            .control
            .handle()
            .ok_or(HubError::RuntimeUnavailable)?;
        Ok((binding.identity.clone(), handle))
    }

    fn bound_handle_for(&self, expected: &LiveBinding) -> Result<CodingRuntimeHandle, HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let binding = state.binding.as_ref().ok_or(HubError::Unbound)?;
        if binding.identity.id != expected.id
            || binding.identity.generation != expected.generation
            || binding.identity.session_id != expected.session_id
            || binding.identity.working_dir != expected.working_dir
        {
            return Err(HubError::StaleBinding);
        }
        let status = binding.control.status();
        if status.generation != binding.identity.generation {
            return Err(HubError::RuntimeGenerationChanged {
                expected: binding.identity.generation,
                actual: status.generation,
            });
        }
        binding.control.handle().ok_or(HubError::RuntimeUnavailable)
    }

    fn commit_reconfigure_generation(
        &self,
        binding: &LiveBinding,
        generation: atomcode_coding::RuntimeGeneration,
        provider: Option<(String, String)>,
    ) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_mut().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id
            || current.identity.session_id != binding.session_id
            || current.identity.working_dir != binding.working_dir
        {
            return Err(HubError::StaleBinding);
        }
        let actual = current.control.status().generation;
        if actual != generation.0 {
            return Err(HubError::RuntimeGenerationChanged {
                expected: generation.0,
                actual,
            });
        }
        if current.identity.generation > generation.0 {
            return Err(HubError::RuntimeGenerationChanged {
                expected: generation.0,
                actual: current.identity.generation,
            });
        }
        if let Some((provider, fingerprint)) = provider {
            current.identity.provider = provider;
            current.identity.provider_fingerprint = fingerprint;
        }
        if current.identity.generation < generation.0 {
            current.identity.generation = generation.0;
            state.replay.clear();
            state.goal_progress = None;
            state.pending_requests.clear();
            state.pending_web_steers.clear();
            state.turn_active = false;
            state.last_runtime_sequence = None;
        }
        Ok(())
    }

    async fn commit_changed_snapshot(
        &self,
        binding: &LiveBinding,
        handle: &CodingRuntimeHandle,
        changed: &atomcode_coding::SessionChanged,
    ) -> Result<(), HubError> {
        let snapshot = handle
            .snapshot()
            .await
            .map_err(|error| HubError::SnapshotUnavailable(error.to_string()))?;
        let session_id = changed
            .session_id
            .clone()
            .ok_or_else(|| HubError::SnapshotUnavailable("runtime has no session id".into()))?;
        self.commit_runtime_snapshot(
            binding,
            session_id,
            changed.working_dir.clone(),
            snapshot.as_ref().clone(),
        )?;
        Ok(())
    }

    fn publish_view_locked(&self, state: &mut HubState, event: LiveViewEvent, replay: bool) {
        let Some(binding) = state.binding.as_ref() else {
            return;
        };
        state.next_cursor += 1;
        let observation = LiveObservation {
            binding_id: binding.identity.id,
            generation: binding.identity.generation,
            cursor: state.next_cursor,
            event,
        };
        if replay {
            state.replay.push(observation.clone());
        }
        let _ = self.events.send(observation);
    }

    /// Resolve one pending request after its response has been accepted by the
    /// runtime command boundary. Remove the original request from replay before
    /// broadcasting the terminal so a reconnect cannot resurrect a stale prompt.
    fn resolve_request_locked(&self, state: &mut HubState, id: RequestId) -> Result<(), HubError> {
        let kind = state
            .pending_requests
            .remove(&id)
            .ok_or(HubError::UnknownRequest(id))?;
        state.replay.retain(|observation| {
            !matches!(
                &observation.event,
                LiveViewEvent::Runtime(CodingRuntimeEvent::Request(request))
                    if request.id == id
            )
        });
        self.publish_view_locked(
            state,
            LiveViewEvent::RequestResolved {
                request_id: id,
                kind,
            },
            false,
        );
        Ok(())
    }
}

fn map_session_transition_error(error: atomcode_coding::RuntimeError) -> HubError {
    match error {
        atomcode_coding::RuntimeError::Busy => HubError::ActiveTurn,
        error => HubError::RuntimeRejected(error.to_string()),
    }
}

fn session_change_is_noop(
    binding: &LiveBinding,
    changed: &atomcode_coding::SessionChanged,
) -> bool {
    changed.generation.0 == binding.generation
        && changed.session_id.as_deref() == Some(binding.session_id.as_str())
        && changed.working_dir == binding.working_dir
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use atomcode_coding::{
        CodingRuntimeEvent, DriverCommand, GoalPhase, GoalProgress, RuntimePhase, RuntimeStatus,
        RuntimeUnavailable, SequencedRuntimeEvent, TurnCompletion, UserInput,
    };
    use atomcode_kernel::event::AgentEvent;
    use atomcode_kernel::message::{Message, SessionSnapshot};

    use super::{
        correlate_web_steers_locked, map_session_transition_error, session_change_is_noop,
        HubError, HubState, LiveBinding, LiveRuntimeControl, LiveViewEvent, LiveViewHub,
        PendingWebSteer,
    };

    #[test]
    fn web_steer_correlation_uses_runtime_payload_and_returns_client_identity() {
        let runtime_input = UserInput {
            text: "look\n\n[图片内容（由 vl 识别）]\na cat".into(),
            images: Vec::new(),
        };
        let mut state = HubState::default();
        state.pending_web_steers.push_back(PendingWebSteer {
            runtime_input: runtime_input.clone(),
            client_input_id: "web-1".into(),
        });
        let folded = vec![atomcode_kernel::event::SteeredInput {
            text: runtime_input.text,
            images: runtime_input.images,
        }];

        assert_eq!(
            correlate_web_steers_locked(&mut state, &folded),
            vec![Some("web-1".into())]
        );
        assert!(state.pending_web_steers.is_empty());
    }

    #[test]
    fn late_join_receives_goal_state_after_turn_replay_is_empty() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("one"), control)
            .unwrap();

        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::GoalChanged(GoalProgress {
                    active: true,
                    terminal: None,
                    phase: GoalPhase::Pursuing,
                    round: 0,
                    max_rounds: Some(3),
                    elapsed_secs: 12,
                    condition: "finish the task".into(),
                    last_reason: None,
                }),
            },
        )
        .unwrap();

        let join = hub.join().unwrap();
        assert!(join.replay.is_empty());
        assert_eq!(join.goal_progress.unwrap().elapsed_secs, 12);
    }

    #[test]
    fn seeding_goal_does_not_consume_runtime_sequence_zero() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("one"), control)
            .unwrap();
        hub.seed_goal_progress(
            &binding,
            GoalProgress {
                active: true,
                terminal: None,
                phase: GoalPhase::Pursuing,
                round: 0,
                max_rounds: None,
                elapsed_secs: 0,
                condition: "finish".into(),
                last_reason: None,
            },
        )
        .unwrap();

        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 0,
                event: CodingRuntimeEvent::GoalChanged(GoalProgress {
                    active: true,
                    terminal: None,
                    phase: GoalPhase::Pursuing,
                    round: 1,
                    max_rounds: None,
                    elapsed_secs: 1,
                    condition: "finish".into(),
                    last_reason: None,
                }),
            },
        )
        .expect("the first runtime event must not be treated as stale");
    }

    #[derive(Clone)]
    struct FakeControl {
        status: Arc<Mutex<RuntimeStatus>>,
        commands: Arc<Mutex<Vec<DriverCommand>>>,
    }

    impl LiveRuntimeControl for FakeControl {
        fn status(&self) -> RuntimeStatus {
            *self.status.lock().unwrap()
        }

        fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn handle(&self) -> Option<atomcode_coding::CodingRuntimeHandle> {
            None
        }
    }

    fn control() -> (Arc<FakeControl>, Arc<Mutex<Vec<DriverCommand>>>) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(FakeControl {
                status: Arc::new(Mutex::new(RuntimeStatus {
                    generation: 1,
                    phase: RuntimePhase::Ready,
                })),
                commands: commands.clone(),
            }),
            commands,
        )
    }

    fn snapshot(text: &str) -> SessionSnapshot {
        SessionSnapshot::new(vec![Message::user(text)])
    }

    #[test]
    fn same_generation_and_identity_is_a_noop_session_change() {
        let binding = LiveBinding {
            id: 1,
            generation: 7,
            session_id: "session-1".into(),
            working_dir: PathBuf::from("/project"),
            provider: "test".into(),
            provider_fingerprint: "test".into(),
        };
        let unchanged = atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(7),
            session_id: Some("session-1".into()),
            working_dir: PathBuf::from("/project"),
        };
        assert!(session_change_is_noop(&binding, &unchanged));

        let changed = atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(8),
            ..unchanged
        };
        assert!(!session_change_is_noop(&binding, &changed));
    }

    #[test]
    fn unbound_controls_fail_explicitly() {
        let hub = LiveViewHub::new();
        let error = hub
            .submit(UserInput {
                text: "hello".into(),
                images: Vec::new(),
            })
            .unwrap_err();
        assert_eq!(error, HubError::Unbound);
    }

    #[tokio::test]
    async fn confirmed_submit_requires_an_authoritative_runtime_handle() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("one"), control)
            .unwrap();

        let error = hub
            .submit_confirmed(UserInput::from("hello"))
            .await
            .unwrap_err();

        assert_eq!(error, HubError::RuntimeUnavailable);
        assert!(commands.lock().unwrap().is_empty());
        assert!(hub.join().unwrap().replay.is_empty());
    }

    #[test]
    fn stale_binding_and_sequence_are_rejected() {
        let hub = LiveViewHub::new();
        let (first_control, _) = control();
        let first = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("one"),
                first_control,
            )
            .unwrap();
        let (second_control, _) = control();
        let second = hub
            .bind(
                "session-2",
                PathBuf::from("/two"),
                snapshot("two"),
                second_control,
            )
            .unwrap();

        let stale = hub
            .publish(
                &first,
                SequencedRuntimeEvent {
                    generation: 1,
                    sequence: 1,
                    event: CodingRuntimeEvent::Agent(AgentEvent::TurnStarted),
                },
            )
            .unwrap_err();
        assert_eq!(stale, HubError::StaleBinding);

        hub.publish(
            &second,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 2,
                event: CodingRuntimeEvent::Agent(AgentEvent::TurnStarted),
            },
        )
        .unwrap();
        let duplicate = hub
            .publish(
                &second,
                SequencedRuntimeEvent {
                    generation: 1,
                    sequence: 2,
                    event: CodingRuntimeEvent::Agent(AgentEvent::TextDelta("late".into())),
                },
            )
            .unwrap_err();
        assert_eq!(duplicate, HubError::StaleEvent);
    }

    #[test]
    fn pending_request_is_correlated_and_consumed_once() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("one"), control)
            .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Request(atomcode_coding::RuntimeRequest {
                    id: 42,
                    kind: "approval".into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                }),
            },
        )
        .unwrap();
        let mut live = hub.join().unwrap();
        assert!(live.replay.iter().any(|observation| matches!(
            &observation.event,
            LiveViewEvent::Runtime(CodingRuntimeEvent::Request(request)) if request.id == 42
        )));

        assert_eq!(
            hub.respond(7, serde_json::Value::Null).unwrap_err(),
            HubError::UnknownRequest(7)
        );
        hub.respond(42, serde_json::json!({ "decision": "allow" }))
            .unwrap();
        assert_eq!(
            hub.respond(42, serde_json::Value::Null).unwrap_err(),
            HubError::UnknownRequest(42)
        );
        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [DriverCommand::Respond { id: 42, .. }]
        ));
        let resolved = live
            .receiver
            .try_recv()
            .expect("peer sees request terminal");
        assert!(matches!(
            resolved.event,
            LiveViewEvent::RequestResolved {
                request_id: 42,
                ref kind,
            } if kind == "approval"
        ));
        assert!(
            hub.join()
                .unwrap()
                .replay
                .iter()
                .all(|observation| !matches!(
                    &observation.event,
                    LiveViewEvent::Runtime(CodingRuntimeEvent::Request(request)) if request.id == 42
                )),
            "a reconnect must not replay an already-resolved request"
        );
    }

    #[test]
    fn terminal_snapshot_replaces_replay_atomically() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Agent(AgentEvent::TurnStarted),
            },
        )
        .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 2,
                event: CodingRuntimeEvent::Agent(AgentEvent::TextDelta("new".into())),
            },
        )
        .unwrap();
        let during = hub.join().unwrap();
        assert_eq!(during.snapshot.messages[0].text, "old");
        assert_eq!(during.replay.len(), 2);

        let committed = snapshot("committed");
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 3,
                event: CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: atomcode_kernel::event::StopReason::Stopped,
                    snapshot: Arc::new(committed),
                    stats: Default::default(),
                }),
            },
        )
        .unwrap();
        let after = hub.join().unwrap();
        assert_eq!(after.snapshot.messages[0].text, "committed");
        assert!(after.replay.is_empty());
        assert!(matches!(
            during.replay[0].event,
            LiveViewEvent::Runtime(CodingRuntimeEvent::Agent(AgentEvent::TurnStarted))
        ));
    }

    #[test]
    fn generation_advance_invalidates_pending_interactions() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Request(atomcode_coding::RuntimeRequest {
                    id: 42,
                    kind: "approval".into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                }),
            },
        )
        .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 2,
                sequence: 2,
                event: CodingRuntimeEvent::Reconfigured {
                    operation: atomcode_coding::ReconfigureKind::Provider,
                },
            },
        )
        .unwrap();

        assert_eq!(hub.join().unwrap().binding.generation, 2);
        assert_eq!(
            hub.respond(42, serde_json::Value::Null).unwrap_err(),
            HubError::UnknownRequest(42)
        );
        assert_eq!(
            hub.publish(
                &binding,
                SequencedRuntimeEvent {
                    generation: 1,
                    sequence: 3,
                    event: CodingRuntimeEvent::Agent(AgentEvent::Warning("late".into())),
                },
            )
            .unwrap_err(),
            HubError::StaleEvent
        );
    }

    #[test]
    fn cancel_requires_an_active_turn() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        assert_eq!(hub.cancel().unwrap_err(), HubError::NoActiveTurn);

        hub.submit(UserInput {
            text: "hello".into(),
            images: Vec::new(),
        })
        .unwrap();
        hub.cancel().unwrap();
        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [DriverCommand::Submit(_), DriverCommand::Cancel]
        ));
    }

    #[test]
    fn missing_terminal_snapshot_fails_join_closed() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                    turn_id: 1,
                    reason: atomcode_kernel::event::StopReason::ProviderError,
                    error: atomcode_coding::RuntimeSnapshotError {
                        message: "snapshot failed".into(),
                    },
                    stats: Default::default(),
                }),
            },
        )
        .unwrap();

        let error = match hub.join() {
            Ok(_) => panic!("join must fail without an authoritative terminal snapshot"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            HubError::SnapshotUnavailable("snapshot failed".into())
        );
    }

    #[test]
    fn active_binding_cannot_be_replaced() {
        let hub = LiveViewHub::new();
        let (first_control, _) = control();
        hub.bind(
            "session-1",
            PathBuf::from("/one"),
            snapshot("old"),
            first_control,
        )
        .unwrap();
        hub.submit(UserInput {
            text: "running".into(),
            images: Vec::new(),
        })
        .unwrap();
        let (second_control, _) = control();

        let error = hub
            .bind(
                "session-2",
                PathBuf::from("/two"),
                snapshot("new"),
                second_control,
            )
            .unwrap_err();
        assert_eq!(error, HubError::ActiveTurn);
    }

    #[test]
    fn runtime_already_in_turn_cannot_be_bound_without_replay_state() {
        let hub = LiveViewHub::new();
        let control = Arc::new(FakeControl {
            status: Arc::new(Mutex::new(RuntimeStatus {
                generation: 1,
                phase: RuntimePhase::InTurn,
            })),
            commands: Arc::new(Mutex::new(Vec::new())),
        });

        assert_eq!(
            hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
                .unwrap_err(),
            HubError::ActiveTurn
        );
    }

    #[test]
    fn unbind_is_scoped_to_the_current_binding() {
        let hub = LiveViewHub::new();
        let (first_control, _) = control();
        let first = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("one"),
                first_control,
            )
            .unwrap();
        let (second_control, _) = control();
        let second = hub
            .bind(
                "session-2",
                PathBuf::from("/two"),
                snapshot("two"),
                second_control,
            )
            .unwrap();

        assert_eq!(hub.unbind(&first).unwrap_err(), HubError::StaleBinding);
        hub.unbind(&second).unwrap();
        assert_eq!(
            hub.submit(UserInput {
                text: "orphan".into(),
                images: Vec::new(),
            })
            .unwrap_err(),
            HubError::Unbound
        );
    }

    #[test]
    fn turn_in_progress_reflects_runtime_phase_and_gates_provider_reload() {
        let hub = LiveViewHub::new();
        // Unbound: nothing to interrupt.
        assert!(!hub.turn_in_progress());

        let (control, _) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control.clone())
            .unwrap();
        // Ready phase, no active turn → a provider reload is safe.
        assert!(!hub.turn_in_progress());

        // A running turn (InTurn phase) must gate the reload.
        control.status.lock().unwrap().phase = RuntimePhase::InTurn;
        assert!(hub.turn_in_progress());

        // Parked awaiting approval also counts as in-flight.
        control.status.lock().unwrap().phase = RuntimePhase::WaitingApproval;
        assert!(hub.turn_in_progress());

        // Already reconfiguring (e.g. a prior reload in flight): a second reload
        // must also be refused — mirrors bind_with_provider's active set.
        control.status.lock().unwrap().phase = RuntimePhase::Reconfiguring;
        assert!(hub.turn_in_progress());
    }

    #[test]
    fn busy_session_transition_maps_to_active_turn() {
        assert_eq!(
            map_session_transition_error(atomcode_coding::RuntimeError::Busy),
            HubError::ActiveTurn
        );
        assert_eq!(
            map_session_transition_error(atomcode_coding::RuntimeError::Unavailable),
            HubError::RuntimeRejected("coding runtime is unavailable".into())
        );
    }

    #[tokio::test]
    async fn fresh_session_rejects_a_stale_expected_binding() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let current = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        let mut stale = current.clone();
        stale.session_id = "session-2".into();

        assert_eq!(hub.fresh_session(&stale).await, Err(HubError::StaleBinding));
        assert_eq!(hub.binding().unwrap(), current);
    }

    #[test]
    fn driver_commands_and_local_inputs_share_the_bound_runtime() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();

        hub.dispatch(DriverCommand::SetMode(atomcode_coding::RuntimeMode::Plan))
            .unwrap();
        hub.accept_local_input(UserInput::from("typed in tui"))
            .unwrap();

        let join = hub.join().unwrap();
        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [DriverCommand::SetMode(atomcode_coding::RuntimeMode::Plan)]
        ));
        assert!(matches!(
            join.replay.as_slice(),
            [observation]
                if matches!(&observation.event, LiveViewEvent::InputAccepted { input, .. }
                    if input.text == "typed in tui")
        ));
    }

    #[test]
    fn dispatch_routes_manual_compact_to_bound_runtime() {
        let hub = LiveViewHub::new();
        let (control, commands) = control();
        hub.bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();

        hub.dispatch(DriverCommand::Compact(None)).unwrap();

        assert!(matches!(
            commands.lock().unwrap().as_slice(),
            [DriverCommand::Compact(None)]
        ));
    }

    #[test]
    fn dispatch_compact_without_runtime_is_unbound() {
        let hub = LiveViewHub::new();
        let error = hub.dispatch(DriverCommand::Compact(None)).unwrap_err();
        assert_eq!(error, HubError::Unbound);
    }

    #[test]
    fn session_change_fails_join_closed_until_matching_snapshot_is_committed() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();

        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::SessionChanged(atomcode_coding::SessionChanged {
                    generation: atomcode_coding::RuntimeGeneration(1),
                    session_id: Some("session-2".into()),
                    working_dir: PathBuf::from("/two"),
                }),
            },
        )
        .unwrap();

        assert_eq!(
            match hub.join() {
                Ok(_) => panic!("join must not expose the previous session snapshot"),
                Err(error) => error,
            },
            HubError::SnapshotUnavailable("session snapshot pending".into())
        );
        hub.commit_runtime_snapshot(
            &binding,
            "session-2".into(),
            PathBuf::from("/two"),
            snapshot("new"),
        )
        .unwrap();
        let join = hub.join().unwrap();
        assert_eq!(join.binding.session_id, "session-2");
        assert_eq!(join.binding.working_dir, PathBuf::from("/two"));
        assert_eq!(join.binding.generation, 1);
        assert_eq!(join.snapshot.messages[0].text, "new");
    }

    #[test]
    fn provider_join_rejects_a_stale_session_atomically() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        hub.bind(
            "session-1",
            PathBuf::from("/one"),
            snapshot("committed"),
            control,
        )
        .unwrap();

        assert_eq!(
            match hub.join_for_provider(Some("session-2")) {
                Ok(_) => panic!("a provider request must not switch sessions"),
                Err(error) => error,
            },
            HubError::StaleBinding,
        );
        assert_eq!(hub.binding().unwrap().session_id, "session-1");
    }

    #[test]
    fn reconfigure_event_for_same_session_keeps_committed_snapshot_available() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("committed"),
                control.clone(),
            )
            .unwrap();

        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 2,
                sequence: 1,
                event: CodingRuntimeEvent::SessionChanged(atomcode_coding::SessionChanged {
                    generation: atomcode_coding::RuntimeGeneration(2),
                    session_id: Some("session-1".into()),
                    working_dir: PathBuf::from("/one"),
                }),
            },
        )
        .unwrap();

        let join = hub
            .join()
            .expect("same-session reconfigure keeps snapshot valid");
        assert_eq!(join.binding.generation, 2);
        assert_eq!(join.snapshot.messages[0].text, "committed");
    }

    #[test]
    fn confirmed_reconfigure_commits_generation_before_followup_commands() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("committed"),
                control.clone(),
            )
            .unwrap();
        *control.status.lock().unwrap() = RuntimeStatus {
            generation: 2,
            phase: RuntimePhase::Ready,
        };

        hub.commit_reconfigure_generation(&binding, atomcode_coding::RuntimeGeneration(2), None)
            .unwrap();

        assert_eq!(hub.join().unwrap().binding.generation, 2);
    }

    #[test]
    fn provider_commit_rejects_a_binding_whose_session_changed() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("committed"),
                control.clone(),
            )
            .unwrap();
        *control.status.lock().unwrap() = RuntimeStatus {
            generation: 2,
            phase: RuntimePhase::Ready,
        };
        hub.replace_snapshot(
            &binding,
            "session-2".into(),
            PathBuf::from("/two"),
            snapshot("new"),
        )
        .unwrap();

        assert_eq!(
            hub.commit_reconfigure_generation(
                &binding,
                atomcode_coding::RuntimeGeneration(2),
                Some(("provider-b".into(), "fingerprint-b".into())),
            )
            .unwrap_err(),
            HubError::StaleBinding,
        );
        assert_ne!(hub.binding().unwrap().provider, "provider-b");
    }

    #[test]
    fn confirmed_reconfigure_preserves_sequence_when_event_arrived_first() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind(
                "session-1",
                PathBuf::from("/one"),
                snapshot("committed"),
                control.clone(),
            )
            .unwrap();
        *control.status.lock().unwrap() = RuntimeStatus {
            generation: 2,
            phase: RuntimePhase::Ready,
        };
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 2,
                sequence: 2,
                event: CodingRuntimeEvent::ProviderChanged {
                    provider: "provider".into(),
                    model: "model".into(),
                },
            },
        )
        .unwrap();

        hub.commit_reconfigure_generation(&binding, atomcode_coding::RuntimeGeneration(2), None)
            .unwrap();

        let error = hub
            .publish(
                &binding,
                SequencedRuntimeEvent {
                    generation: 2,
                    sequence: 1,
                    event: CodingRuntimeEvent::ProviderChanged {
                        provider: "stale".into(),
                        model: "stale".into(),
                    },
                },
            )
            .unwrap_err();
        assert_eq!(error, HubError::StaleEvent);
    }

    #[test]
    fn provider_event_updates_the_bound_runtime_projection() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind_with_provider(
                "session-1",
                PathBuf::from("/one"),
                "provider-a",
                "fingerprint-a",
                snapshot("committed"),
                control,
            )
            .unwrap();

        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::ProviderChanged {
                    provider: "provider-b".into(),
                    model: "model-b".into(),
                },
            },
        )
        .unwrap();

        let projected = hub.binding().unwrap();
        assert_eq!(projected.provider, "provider-b");
        assert!(projected.provider_fingerprint.is_empty());
    }

    #[test]
    fn authoritative_rebind_replaces_snapshot_and_invalidates_old_requests() {
        let hub = LiveViewHub::new();
        let (control, _) = control();
        let binding = hub
            .bind("session-1", PathBuf::from("/one"), snapshot("old"), control)
            .unwrap();
        hub.publish(
            &binding,
            SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Request(atomcode_coding::RuntimeRequest {
                    id: 42,
                    kind: "approval".into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                }),
            },
        )
        .unwrap();

        hub.replace_snapshot(
            &binding,
            "session-2".into(),
            PathBuf::from("/two"),
            snapshot("new"),
        )
        .unwrap();

        let join = hub.join().unwrap();
        assert_eq!(join.binding.session_id, "session-2");
        assert_eq!(join.snapshot.messages[0].text, "new");
        assert!(join.replay.is_empty());
        assert_eq!(
            hub.respond(42, serde_json::Value::Null).unwrap_err(),
            HubError::UnknownRequest(42)
        );
    }
}
