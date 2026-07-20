use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use atomcode_coding::{
    CodingRuntimeEvent, CodingRuntimeHandle, DriverCommand, RuntimePhase, RuntimeStatus,
    RuntimeUnavailable, SequencedRuntimeEvent, TurnCompletion, UserInput,
};
use atomcode_kernel::event::{AgentEvent, RequestId};
use atomcode_kernel::message::SessionSnapshot;
use tokio::sync::broadcast;

const BROADCAST_CAPACITY: usize = 1024;

pub trait LiveRuntimeControl: Send + Sync {
    fn status(&self) -> RuntimeStatus;
    fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable>;
}

impl LiveRuntimeControl for CodingRuntimeHandle {
    fn status(&self) -> RuntimeStatus {
        CodingRuntimeHandle::status(self)
    }

    fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable> {
        CodingRuntimeHandle::dispatch(self, command)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveBinding {
    pub id: u64,
    pub generation: u64,
    pub session_id: String,
    pub working_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub enum LiveViewEvent {
    InputAccepted(UserInput),
    CommandOutput(String),
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
    pub replay: Vec<LiveObservation>,
    pub receiver: broadcast::Receiver<LiveObservation>,
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
    pending_requests: HashMap<RequestId, String>,
    turn_active: bool,
    last_runtime_sequence: Option<u64>,
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

    pub fn bind(
        &self,
        session_id: impl Into<String>,
        working_dir: PathBuf,
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
        };
        state.binding = Some(BoundRuntime {
            identity: identity.clone(),
            control,
        });
        state.snapshot = Some(Arc::new(snapshot));
        state.snapshot_error = None;
        state.replay.clear();
        state.pending_requests.clear();
        state.turn_active = false;
        state.last_runtime_sequence = None;
        Ok(identity)
    }

    pub fn join(&self) -> Result<LiveJoin, HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let binding = state
            .binding
            .as_ref()
            .ok_or(HubError::Unbound)?
            .identity
            .clone();
        if let Some(error) = &state.snapshot_error {
            return Err(HubError::SnapshotUnavailable(error.clone()));
        }
        let snapshot = state.snapshot.clone().ok_or(HubError::Unbound)?;
        let receiver = self.events.subscribe();
        Ok(LiveJoin {
            binding,
            snapshot,
            replay: state.replay.clone(),
            receiver,
        })
    }

    pub fn unbind(&self, binding: &LiveBinding) -> Result<(), HubError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.binding.as_ref().ok_or(HubError::Unbound)?;
        if current.identity.id != binding.id {
            return Err(HubError::StaleBinding);
        }
        if state.turn_active {
            return Err(HubError::ActiveTurn);
        }
        state.binding = None;
        state.snapshot = None;
        state.snapshot_error = None;
        state.replay.clear();
        state.pending_requests.clear();
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
        state.pending_requests.clear();
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
        }
        state.turn_active = true;
        state.pending_requests.clear();
        self.publish_view_locked(&mut state, LiveViewEvent::InputAccepted(input), true);
        Ok(())
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
        }
        state.turn_active = true;
        state.pending_requests.clear();
        self.publish_view_locked(&mut state, LiveViewEvent::InputAccepted(input), true);
        Ok(())
    }

    pub fn dispatch(&self, command: DriverCommand) -> Result<(), HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Self::dispatch_locked(&state, command)
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
        state.pending_requests.remove(&id);
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
        state.pending_requests.remove(&id);
        Ok(id)
    }

    pub fn cancel(&self) -> Result<(), HubError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.turn_active {
            return Err(HubError::NoActiveTurn);
        }
        Self::dispatch_locked(&state, DriverCommand::Cancel)
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
            state.pending_requests.clear();
            state.turn_active = false;
            state.snapshot_error = None;
        }
        state.last_runtime_sequence = Some(envelope.sequence);

        let event = envelope.event;
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
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable {
                error, ..
            }) => {
                state.snapshot_error = Some(error.message.clone());
                state.pending_requests.clear();
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::RuntimeStopped(_) => {
                state.snapshot_error = Some("runtime stopped".into());
                state.pending_requests.clear();
                state.turn_active = false;
                replay = false;
            }
            CodingRuntimeEvent::SessionChanged(changed) => {
                let current = state.binding.as_mut().expect("binding checked above");
                if let Some(session_id) = &changed.session_id {
                    current.identity.session_id = session_id.clone();
                }
                current.identity.working_dir = changed.working_dir.clone();
                // The identity changes before the owner can asynchronously fetch
                // the replacement snapshot. Never pair it with the previous
                // session's snapshot during that window.
                state.snapshot_error = Some("session snapshot pending".into());
                state.replay.clear();
                state.pending_requests.clear();
                state.turn_active = false;
            }
            CodingRuntimeEvent::WorkingDirectoryChanged(working_dir) => {
                let current = state.binding.as_mut().expect("binding checked above");
                current.identity.working_dir = working_dir.clone();
            }
            _ => {}
        }
        self.publish_view_locked(&mut state, LiveViewEvent::Runtime(event), replay);
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
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use atomcode_coding::{
        CodingRuntimeEvent, DriverCommand, RuntimePhase, RuntimeStatus, RuntimeUnavailable,
        SequencedRuntimeEvent, TurnCompletion, UserInput,
    };
    use atomcode_kernel::event::AgentEvent;
    use atomcode_kernel::message::{Message, SessionSnapshot};

    use super::{HubError, LiveRuntimeControl, LiveViewEvent, LiveViewHub};

    #[derive(Clone)]
    struct FakeControl {
        status: RuntimeStatus,
        commands: Arc<Mutex<Vec<DriverCommand>>>,
    }

    impl LiveRuntimeControl for FakeControl {
        fn status(&self) -> RuntimeStatus {
            self.status
        }

        fn dispatch(&self, command: DriverCommand) -> Result<(), RuntimeUnavailable> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }
    }

    fn control() -> (Arc<FakeControl>, Arc<Mutex<Vec<DriverCommand>>>) {
        let commands = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(FakeControl {
                status: RuntimeStatus {
                    generation: 1,
                    phase: RuntimePhase::Ready,
                },
                commands: commands.clone(),
            }),
            commands,
        )
    }

    fn snapshot(text: &str) -> SessionSnapshot {
        SessionSnapshot::new(vec![Message::user(text)])
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
            status: RuntimeStatus {
                generation: 1,
                phase: RuntimePhase::InTurn,
            },
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
                if matches!(&observation.event, LiveViewEvent::InputAccepted(input)
                    if input.text == "typed in tui")
        ));
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
