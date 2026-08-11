use std::path::PathBuf;

use super::ui_event::UiEvent as AgentEvent;
use crate::session::Session;
use atomcode_coding::runtime::{CodingRuntimeEvent, CompactionCompletion};
use atomcode_config::i18n::{t, Msg};

use super::RuntimeEndpoint;

pub const MAX_BACKGROUND_SLOTS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeId(u64);

impl RuntimeId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

pub struct RuntimeEvent {
    pub runtime_id: RuntimeId,
    pub event: RuntimeEventPayload,
}

#[derive(Clone, Debug)]
pub enum RuntimeEventPayload {
    Ui(AgentEvent),
    Native(CodingRuntimeEvent),
    SequencedNative(atomcode_coding::SequencedRuntimeEvent),
    Driver(DriverEvent),
}

#[derive(Clone, Debug)]
pub enum DriverEvent {
    LocalShellFinished {
        output: String,
        failed: bool,
    },
    SessionTransitionFinished {
        operation: atomcode_coding::ReconfigureKind,
        result: Result<atomcode_coding::SessionChanged, atomcode_coding::RuntimeError>,
    },
    SessionResumePrepared {
        project_bucket: String,
        session_id: String,
        working_dir: PathBuf,
        result: Result<atomcode_daemon::legacy_convert::PreparedCatalogSessionResume, String>,
    },
    CapabilitiesReloadFinished {
        result: Result<atomcode_coding::SessionChanged, atomcode_coding::RuntimeError>,
    },
    /// The `/resume` session catalog finished loading off the UI thread (the scan
    /// reads/parses every session file, which froze the event loop when done
    /// inline). Carries the current-project session list ready to install into the
    /// picker; `working_dir` lets the handler drop a result the user has navigated
    /// away from.
    SessionCatalogLoaded {
        working_dir: PathBuf,
        result: Result<Vec<crate::session::SessionMeta>, String>,
    },
}

pub fn spawn_event_forwarder(
    runtime_id: RuntimeId,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<RuntimeEventPayload>,
    fan_tx: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
) {
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            if fan_tx.send(RuntimeEvent { runtime_id, event }).is_err() {
                break;
            }
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Running,
    Idle,
    Done,
    Cancelled,
    Error,
}

impl RuntimeState {
    /// Return a localised label for the current runtime state.
    pub fn localised(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Running => t(Msg::BgStateRunning),
            Self::Idle => t(Msg::BgStateIdle),
            Self::Done => t(Msg::BgStateDone),
            Self::Cancelled => t(Msg::BgStateCancelled),
            Self::Error => t(Msg::BgStateError),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgError {
    SlotLimit { max: usize },
    InvalidSlot { slot: usize, len: usize },
    NoRuntimeClient { slot: usize },
    SessionProjectionUnavailable { slot: usize, error: String },
}

pub struct ForegroundRuntime {
    pub runtime_id: RuntimeId,
    pub endpoint: Option<RuntimeEndpoint>,
    pub session: Session,
    /// Runtime-owned project directory. `Session::working_dir` is persisted
    /// display metadata and can be stale after legacy migrations.
    pub working_dir: PathBuf,
}

pub struct BackgroundSlot {
    pub runtime_id: RuntimeId,
    pub endpoint: Option<RuntimeEndpoint>,
    pub session: Session,
    /// Physical project bucket owned by this runtime.
    pub working_dir: PathBuf,
    pub state: RuntimeState,
    pub created_at: u64,
    pub summary: String,
    /// Presentation events observed after this runtime left the foreground.
    /// They are replayed synchronously before the shared runtime queue is
    /// consumed again, so a pending request cannot be overtaken by its terminal.
    pub buffered_events: Vec<RuntimeEventPayload>,
    /// Runtime-committed identity that could not yet be projected from the
    /// exact native catalog location. Resume must retry and fail closed rather
    /// than pairing this runtime with the previous session mirror.
    pending_session_change: Option<atomcode_coding::SessionChanged>,
    /// Runtime-owned interactive request that arrived while this slot was in
    /// the background. The request id and payload, not transcript heuristics,
    /// are the authority restored when the slot returns to the foreground.
    pending_request: Option<atomcode_coding::RuntimeRequest>,
}

impl BackgroundSlot {
    fn into_foreground(self) -> ForegroundRuntime {
        ForegroundRuntime {
            runtime_id: self.runtime_id,
            endpoint: self.endpoint,
            session: self.session,
            working_dir: self.working_dir,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BgListRow {
    pub slot: usize,
    pub short_id: String,
    pub state: RuntimeState,
    pub created_at: u64,
    pub summary: String,
}

pub struct BackgroundSlots {
    max_slots: usize,
    slots: Vec<BackgroundSlot>,
}

impl BackgroundSlots {
    pub fn new(max_slots: usize) -> Self {
        Self {
            max_slots,
            slots: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn has_capacity(&self) -> bool {
        self.slots.len() < self.max_slots
    }

    pub fn push_slot(&mut self, slot: BackgroundSlot) -> Result<usize, BgError> {
        if !self.has_capacity() {
            return Err(BgError::SlotLimit {
                max: self.max_slots,
            });
        }
        self.slots.push(slot);
        Ok(self.slots.len())
    }

    pub fn drop_slot(&mut self, slot: usize) -> Result<BackgroundSlot, BgError> {
        if slot == 0 || slot > self.slots.len() {
            return Err(BgError::InvalidSlot {
                slot,
                len: self.slots.len(),
            });
        }
        Ok(self.slots.remove(slot - 1))
    }

    pub fn list_rows(&self) -> Vec<BgListRow> {
        self.slots
            .iter()
            .enumerate()
            .map(|(idx, slot)| BgListRow {
                slot: idx + 1,
                short_id: slot.session.short_id().to_string(),
                state: slot.state,
                created_at: slot.created_at,
                summary: slot.summary.clone(),
            })
            .collect()
    }

    pub fn apply_event_to_slot(&mut self, slot: usize, event: &AgentEvent) -> bool {
        if slot == 0 || slot > self.slots.len() {
            return false;
        }
        let bg = &mut self.slots[slot - 1];
        match event {
            AgentEvent::TurnComplete {
                snapshot,
                stop_reason,
                ..
            } => {
                bg.pending_request = None;
                retain_session_replay_events(&mut bg.buffered_events);
                bg.state = match stop_reason {
                    super::ui_event::UiTurnStopReason::Natural => RuntimeState::Done,
                    super::ui_event::UiTurnStopReason::Cancelled => RuntimeState::Cancelled,
                    _ => RuntimeState::Error,
                };
                super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                bg.summary = session_summary(&bg.session);
                true
            }
            AgentEvent::TurnCancelled { snapshot } => {
                bg.pending_request = None;
                retain_session_replay_events(&mut bg.buffered_events);
                bg.state = RuntimeState::Cancelled;
                super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                bg.summary = session_summary(&bg.session);
                true
            }
            AgentEvent::ApprovalNeeded { snapshot, .. } => {
                // Persist mid-turn messages so /bg <N> can replay the
                // conversation even while the turn is still in progress.
                if !snapshot.messages.is_empty() {
                    super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                    bg.summary = session_summary(&bg.session);
                    retain_session_replay_events(&mut bg.buffered_events);
                }
                false
            }
            AgentEvent::WorkingDirChanged(working_dir) => {
                bg.working_dir = atomcode_capabilities::pathnorm::strip_verbatim_path(working_dir);
                false
            }
            AgentEvent::Error { snapshot, .. } => {
                // Diagnostic only: the runtime-owned TurnFinished or
                // RuntimeStopped event decides the terminal state. Preserve a
                // non-empty snapshot for replay without making the slot idle.
                let did_snapshot = !snapshot.messages.is_empty();
                if did_snapshot {
                    super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                    bg.summary = session_summary(&bg.session);
                    retain_session_replay_events(&mut bg.buffered_events);
                }
                bg.buffered_events
                    .push(RuntimeEventPayload::Ui(event.clone()));
                did_snapshot
            }
            AgentEvent::TextDelta(_)
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallStreaming { .. }
            | AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolOutputChunk { .. }
            | AgentEvent::ToolCallResult { .. } => {
                bg.buffered_events
                    .push(RuntimeEventPayload::Ui(event.clone()));
                false
            }
            AgentEvent::UserEcho(_) => {
                bg.buffered_events
                    .push(RuntimeEventPayload::Ui(event.clone()));
                false
            }
            AgentEvent::SharedRequestResolved { request_id, .. } => {
                if bg
                    .pending_request
                    .as_ref()
                    .is_some_and(|request| request.id == *request_id)
                {
                    bg.pending_request = None;
                }
                false
            }
            _ => false,
        }
    }

    fn slot_for_runtime_id(&self, runtime_id: RuntimeId) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.runtime_id == runtime_id)
            .map(|idx| idx + 1)
    }

    fn slot_mut_for_runtime_id(&mut self, runtime_id: RuntimeId) -> Option<&mut BackgroundSlot> {
        self.slots
            .iter_mut()
            .find(|slot| slot.runtime_id == runtime_id)
    }

    #[cfg(test)]
    pub fn push_test_slot(
        &mut self,
        session: Session,
        state: RuntimeState,
    ) -> Result<usize, BgError> {
        self.push_slot(BackgroundSlot {
            runtime_id: RuntimeId::new(self.slots.len() as u64 + 1),
            endpoint: None,
            summary: session.name.clone(),
            working_dir: session.working_dir.clone(),
            session,
            state,
            created_at: 0,
            buffered_events: Vec::new(),
            pending_session_change: None,
            pending_request: None,
        })
    }
}

pub struct ResumeOutcome {
    pub resumed_session: Session,
    pub resumed_working_dir: PathBuf,
    pub resumed_runtime_id: RuntimeId,
    pub resumed_endpoint: RuntimeEndpoint,
    pub resumed_state: RuntimeState,
    pub replay_events: Vec<RuntimeEventPayload>,
    pub previous_foreground_slot: Option<usize>,
}

pub struct BgRuntimeManager {
    foreground: ForegroundRuntime,
    backgrounds: BackgroundSlots,
    next_runtime_id: u64,
}

fn load_background_session_projection(
    changed: &atomcode_coding::SessionChanged,
) -> Result<Session, String> {
    load_background_session_projection_in_root(
        &atomcode_capabilities::session::SessionManager::sessions_root(),
        changed,
    )
}

fn load_background_session_projection_in_root(
    sessions_root: &std::path::Path,
    changed: &atomcode_coding::SessionChanged,
) -> Result<Session, String> {
    let session_id = changed
        .session_id
        .as_deref()
        .ok_or_else(|| "background runtime changed to a sessionless state".to_string())?;
    let working_dir = atomcode_capabilities::pathnorm::strip_verbatim_path(&changed.working_dir);
    let project_bucket = atomcode_capabilities::session::SessionManager::project_hash(&working_dir);
    let manager = atomcode_capabilities::session::SessionManager::with_root(
        sessions_root.join(&project_bucket),
    );
    let loaded = manager.load_native_session(session_id).map_err(|error| {
        format!("failed to load background session {project_bucket}/{session_id}: {error}")
    })?;
    let session = Session::from_catalog_view(loaded.into())
        .map_err(|error| format!("failed to decode background session {session_id}: {error}"))?;
    if session.id != session_id {
        return Err(format!(
            "background session identity mismatch: runtime={session_id:?}, catalog={:?}",
            session.id
        ));
    }
    Ok(session)
}

fn resolve_background_session_projection(
    changed: &atomcode_coding::SessionChanged,
    load: impl FnOnce(&atomcode_coding::SessionChanged) -> Result<Session, String>,
) -> Result<(Session, PathBuf), String> {
    let expected_id = changed
        .session_id
        .as_deref()
        .ok_or_else(|| "background runtime changed to a sessionless state".to_string())?;
    let session = load(changed)?;
    if session.id != expected_id {
        return Err(format!(
            "background session identity mismatch: runtime={expected_id:?}, loaded={:?}",
            session.id
        ));
    }
    Ok((
        session,
        atomcode_capabilities::pathnorm::strip_verbatim_path(&changed.working_dir),
    ))
}

impl BgRuntimeManager {
    pub fn new(
        session: Session,
        working_dir: PathBuf,
        runtime_id: RuntimeId,
        endpoint: RuntimeEndpoint,
    ) -> Self {
        Self {
            foreground: ForegroundRuntime {
                runtime_id,
                endpoint: Some(endpoint),
                session,
                working_dir,
            },
            backgrounds: BackgroundSlots::new(MAX_BACKGROUND_SLOTS),
            next_runtime_id: runtime_id.0,
        }
    }

    pub fn allocate_runtime_id(&mut self) -> RuntimeId {
        self.next_runtime_id = self.next_runtime_id.saturating_add(1);
        RuntimeId::new(self.next_runtime_id)
    }

    pub fn backgrounds(&self) -> &BackgroundSlots {
        &self.backgrounds
    }

    pub fn has_capacity(&self) -> bool {
        self.backgrounds.has_capacity()
    }

    pub fn set_foreground_session(&mut self, session: Session, working_dir: PathBuf) {
        self.foreground.session = session;
        self.foreground.working_dir = working_dir;
    }

    pub fn set_foreground_runtime(
        &mut self,
        runtime_id: RuntimeId,
        endpoint: RuntimeEndpoint,
        session: Session,
        working_dir: PathBuf,
    ) {
        self.foreground = ForegroundRuntime {
            runtime_id,
            endpoint: Some(endpoint),
            session,
            working_dir,
        };
    }

    pub fn background_current(
        &mut self,
        new_endpoint: RuntimeEndpoint,
        new_session: Session,
        new_working_dir: PathBuf,
        new_runtime_id: RuntimeId,
        current_state: RuntimeState,
    ) -> Result<usize, BgError> {
        self.background_current_with_replay(
            new_endpoint,
            new_session,
            new_working_dir,
            new_runtime_id,
            current_state,
            Vec::new(),
        )
    }

    pub fn background_current_with_replay(
        &mut self,
        new_endpoint: RuntimeEndpoint,
        new_session: Session,
        new_working_dir: PathBuf,
        new_runtime_id: RuntimeId,
        current_state: RuntimeState,
        replay_events: Vec<RuntimeEventPayload>,
    ) -> Result<usize, BgError> {
        if !self.backgrounds.has_capacity() {
            return Err(BgError::SlotLimit {
                max: self.backgrounds.max_slots,
            });
        }
        let old = std::mem::replace(
            &mut self.foreground,
            ForegroundRuntime {
                runtime_id: new_runtime_id,
                endpoint: Some(new_endpoint),
                session: new_session,
                working_dir: new_working_dir,
            },
        );
        let summary = session_summary(&old.session);
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id: old.runtime_id,
            endpoint: old.endpoint,
            session: old.session,
            working_dir: old.working_dir,
            state: current_state,
            created_at: current_timestamp(),
            summary,
            buffered_events: replay_events,
            pending_session_change: None,
            pending_request: None,
        })
    }

    pub fn push_background_runtime(
        &mut self,
        runtime_id: RuntimeId,
        endpoint: RuntimeEndpoint,
        session: Session,
        working_dir: PathBuf,
        state: RuntimeState,
    ) -> Result<usize, BgError> {
        let summary = session_summary(&session);
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id,
            endpoint: Some(endpoint),
            session,
            working_dir,
            state,
            created_at: current_timestamp(),
            summary,
            buffered_events: Vec::new(),
            pending_session_change: None,
            pending_request: None,
        })
    }

    pub fn resume_slot(
        &mut self,
        slot: usize,
        current_state: RuntimeState,
    ) -> Result<ResumeOutcome, BgError> {
        self.resume_slot_with_loader(slot, current_state, load_background_session_projection)
    }

    fn resume_slot_with_loader(
        &mut self,
        slot: usize,
        current_state: RuntimeState,
        load: impl FnOnce(&atomcode_coding::SessionChanged) -> Result<Session, String>,
    ) -> Result<ResumeOutcome, BgError> {
        if slot == 0 || slot > self.backgrounds.slots.len() {
            return Err(BgError::InvalidSlot {
                slot,
                len: self.backgrounds.slots.len(),
            });
        }
        let index = slot - 1;
        let resumed_endpoint = self.backgrounds.slots[index]
            .endpoint
            .clone()
            .ok_or(BgError::NoRuntimeClient { slot })?;
        if let Some(changed) = self.backgrounds.slots[index].pending_session_change.clone() {
            let (session, working_dir) = resolve_background_session_projection(&changed, load)
                .map_err(|error| BgError::SessionProjectionUnavailable { slot, error })?;
            let background = &mut self.backgrounds.slots[index];
            background.summary = session_summary(&session);
            background.session = session;
            background.working_dir = working_dir;
            background.pending_session_change = None;
        }

        let mut resumed = self.backgrounds.slots.remove(index);
        let resumed_state = resumed.state;
        let pending_request = resumed.pending_request.take();
        let mut replay_events = std::mem::take(&mut resumed.buffered_events);
        if let Some(request) = pending_request {
            replay_events.push(RuntimeEventPayload::Native(CodingRuntimeEvent::Request(
                request,
            )));
        }
        let old_foreground = std::mem::replace(&mut self.foreground, resumed.into_foreground());
        let old_had_state = !old_foreground.session.messages.is_empty()
            || matches!(current_state, RuntimeState::Running);
        let previous_foreground_slot = if old_had_state {
            let summary = session_summary(&old_foreground.session);
            self.backgrounds.slots.push(BackgroundSlot {
                runtime_id: old_foreground.runtime_id,
                endpoint: old_foreground.endpoint,
                session: old_foreground.session,
                working_dir: old_foreground.working_dir,
                state: current_state,
                created_at: current_timestamp(),
                summary,
                buffered_events: Vec::new(),
                pending_session_change: None,
                pending_request: None,
            });
            Some(self.backgrounds.slots.len())
        } else {
            None
        };

        Ok(ResumeOutcome {
            resumed_session: self.foreground.session.clone(),
            resumed_working_dir: self.foreground.working_dir.clone(),
            resumed_runtime_id: self.foreground.runtime_id,
            resumed_endpoint,
            resumed_state,
            replay_events,
            previous_foreground_slot,
        })
    }

    fn apply_background_session_changed_with(
        &mut self,
        runtime_id: RuntimeId,
        changed: atomcode_coding::SessionChanged,
        load: impl FnOnce(&atomcode_coding::SessionChanged) -> Result<Session, String>,
    ) {
        if self
            .backgrounds
            .slot_mut_for_runtime_id(runtime_id)
            .is_none()
        {
            return;
        }
        match resolve_background_session_projection(&changed, load) {
            Ok((session, working_dir)) => {
                let background = self
                    .backgrounds
                    .slot_mut_for_runtime_id(runtime_id)
                    .expect("background slot existence was checked above");
                background.summary = session_summary(&session);
                background.session = session;
                background.working_dir = working_dir;
                background.pending_session_change = None;
                background.pending_request = None;
                background.buffered_events.clear();
            }
            Err(error) => {
                crate::tuix_trace!(
                    "BG",
                    "background session projection deferred runtime={} session={:?} dir={} error={}",
                    runtime_id.0,
                    changed.session_id,
                    changed.working_dir.display(),
                    error,
                );
                let background = self
                    .backgrounds
                    .slot_mut_for_runtime_id(runtime_id)
                    .expect("background slot existence was checked above");
                background.pending_session_change = Some(changed);
                background.pending_request = None;
                background.buffered_events.clear();
            }
        }
    }

    fn apply_background_session_name_with(
        &mut self,
        runtime_id: RuntimeId,
        name: String,
        persist: impl FnOnce(&std::path::Path, &str, &str) -> Result<bool, String>,
    ) {
        let Some(background) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) else {
            return;
        };
        if background.pending_session_change.is_some() {
            background.buffered_events.push(RuntimeEventPayload::Native(
                CodingRuntimeEvent::SessionNameSuggested { name },
            ));
            return;
        }
        if !atomcode_coding::session_title::should_accept_ai_name(
            background.session.user_renamed,
            background.session.ai_named,
        ) {
            return;
        }
        let working_dir = background.working_dir.clone();
        let session_id = background.session.id.clone();

        match persist(&working_dir, session_id.as_str(), &name) {
            Ok(true) => {
                let background = self
                    .backgrounds
                    .slot_mut_for_runtime_id(runtime_id)
                    .expect("background slot existed before synchronous name persistence");
                background.session.name = name;
                background.session.ai_named = true;
                background.session.touch();
                background.summary = session_summary(&background.session);
            }
            Ok(false) => {}
            Err(error) => {
                crate::tuix_trace!(
                    "BG",
                    "background session name persistence deferred runtime={} session={} error={}",
                    runtime_id.0,
                    session_id,
                    error,
                );
                let background = self
                    .backgrounds
                    .slot_mut_for_runtime_id(runtime_id)
                    .expect("background slot existed before synchronous name persistence");
                background.buffered_events.push(RuntimeEventPayload::Native(
                    CodingRuntimeEvent::SessionNameSuggested { name },
                ));
            }
        }
    }

    fn apply_background_session_name(&mut self, runtime_id: RuntimeId, name: String) {
        self.apply_background_session_name_with(
            runtime_id,
            name,
            |working_dir, session_id, name| {
                let project_bucket =
                    atomcode_capabilities::session::SessionManager::project_hash(working_dir);
                atomcode_daemon::legacy_convert::apply_ai_catalog_name_in_project(
                    &project_bucket,
                    session_id,
                    name,
                )
                .map_err(|error| error.to_string())
            },
        );
    }

    pub fn drop_slot(&mut self, slot: usize) -> Result<BackgroundSlot, BgError> {
        self.backgrounds.drop_slot(slot)
    }

    pub fn apply_background_event(&mut self, runtime_id: RuntimeId, event: RuntimeEventPayload) {
        let Some(slot) = self.backgrounds.slot_for_runtime_id(runtime_id) else {
            return;
        };
        let event = match event {
            RuntimeEventPayload::SequencedNative(envelope) => {
                RuntimeEventPayload::Native(envelope.event)
            }
            event => event,
        };
        let terminal = match event {
            RuntimeEventPayload::Ui(event) => self.backgrounds.apply_event_to_slot(slot, &event),
            RuntimeEventPayload::Native(CodingRuntimeEvent::WorkingDirectoryChanged(
                working_dir,
            )) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    bg.working_dir =
                        atomcode_capabilities::pathnorm::strip_verbatim_path(&working_dir);
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::SessionChanged(changed)) => {
                self.apply_background_session_changed_with(
                    runtime_id,
                    changed,
                    load_background_session_projection,
                );
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::SessionNameSuggested { name }) => {
                self.apply_background_session_name(runtime_id, name);
                false
            }
            RuntimeEventPayload::Native(event @ CodingRuntimeEvent::NextPromptSuggested { .. }) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    bg.buffered_events.push(RuntimeEventPayload::Native(event));
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(event)) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    if !matches!(
                        &event,
                        atomcode_kernel::event::AgentEvent::Cancelled
                            | atomcode_kernel::event::AgentEvent::Request { .. }
                            | atomcode_kernel::event::AgentEvent::Snapshot { .. }
                            | atomcode_kernel::event::AgentEvent::TurnComplete { .. }
                    ) {
                        bg.buffered_events.push(RuntimeEventPayload::Native(
                            CodingRuntimeEvent::Agent(event),
                        ));
                    }
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::Request(request)) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    if let Some(snapshot) = request.snapshot.as_deref() {
                        super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                        bg.summary = session_summary(&bg.session);
                        retain_session_replay_events(&mut bg.buffered_events);
                    }
                    bg.pending_request = Some(request);
                    bg.state = RuntimeState::Running;
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::TurnFinished(completion)) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    let policy_terminal = matches!(
                        &completion,
                        atomcode_coding::TurnCompletion::Completed {
                            reason: atomcode_kernel::event::StopReason::PolicyDenied,
                            ..
                        }
                    )
                    .then(|| completion.clone());
                    let policy_intervention = policy_terminal.as_ref().and_then(|_| {
                        bg.buffered_events
                            .iter()
                            .find(|event| {
                                matches!(
                                    event,
                                    RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                                        atomcode_kernel::event::AgentEvent::PolicyIntervention {
                                            ..
                                        }
                                    ))
                                )
                            })
                            .cloned()
                    });
                    bg.pending_request = None;
                    match completion {
                        atomcode_coding::TurnCompletion::Completed {
                            reason, snapshot, ..
                        } => {
                            retain_session_replay_events(&mut bg.buffered_events);
                            super::apply_session_snapshot(
                                &mut bg.session,
                                snapshot.as_ref().clone(),
                            );
                            bg.summary = session_summary(&bg.session);
                            bg.state = background_state_for_stop_reason(reason);
                        }
                        atomcode_coding::TurnCompletion::SnapshotUnavailable { error, .. } => {
                            bg.state = RuntimeState::Error;
                            bg.buffered_events.push(RuntimeEventPayload::Native(
                                CodingRuntimeEvent::Agent(
                                    atomcode_kernel::event::AgentEvent::Error {
                                        message: error.message,
                                        http_status: None,
                                        code: None,
                                    },
                                ),
                            ));
                        }
                    }
                    if let Some(completion) = policy_terminal {
                        // The intervention and its authoritative terminal must
                        // be replayed together. The event stores the safe action
                        // contract; the terminal opens it only after the runtime
                        // is idle again.
                        bg.buffered_events.extend(policy_intervention);
                        bg.buffered_events.push(RuntimeEventPayload::Native(
                            CodingRuntimeEvent::TurnFinished(completion),
                        ));
                    }
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::RuntimeStopped(_)) => {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    bg.pending_request = None;
                    if matches!(bg.state, RuntimeState::Running) {
                        bg.state = RuntimeState::Error;
                    }
                }
                false
            }
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished { completion })
                if completion.is_manual() =>
            {
                if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                    let mut failed = matches!(&completion, CompactionCompletion::Failed { .. });
                    if let CompactionCompletion::Completed(outcome) = &completion {
                        if outcome.committed {
                            if let Some(snapshot) = outcome.committed_snapshot.as_deref() {
                                super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                                // The background CodingRuntime owns native persistence.
                            } else {
                                failed = true;
                            }
                        }
                    }
                    if matches!(bg.state, RuntimeState::Running) {
                        bg.state = if failed {
                            RuntimeState::Error
                        } else {
                            RuntimeState::Idle
                        };
                    }
                }
                false
            }
            RuntimeEventPayload::Native(_) => false,
            RuntimeEventPayload::SequencedNative(_) => unreachable!("normalized above"),
            RuntimeEventPayload::Driver(_) => false,
        };
        let _ = terminal;
    }

    #[cfg(test)]
    pub fn new_for_test(session: Session) -> Self {
        let working_dir = session.working_dir.clone();
        Self::new_for_test_at(session, working_dir)
    }

    #[cfg(test)]
    pub fn new_for_test_at(session: Session, working_dir: PathBuf) -> Self {
        Self {
            foreground: ForegroundRuntime {
                runtime_id: RuntimeId::new(1),
                endpoint: Some(test_endpoint()),
                session,
                working_dir,
            },
            backgrounds: BackgroundSlots::new(MAX_BACKGROUND_SLOTS),
            next_runtime_id: 1,
        }
    }

    #[cfg(test)]
    pub fn foreground_session(&self) -> &Session {
        &self.foreground.session
    }

    #[cfg(test)]
    pub fn foreground_session_mut(&mut self) -> &mut Session {
        &mut self.foreground.session
    }

    #[cfg(test)]
    pub fn foreground_working_dir(&self) -> &PathBuf {
        &self.foreground.working_dir
    }

    #[cfg(test)]
    pub fn push_test_background(
        &mut self,
        session: Session,
        state: RuntimeState,
    ) -> Result<usize, BgError> {
        let runtime_id = self.allocate_runtime_id();
        let summary = session_summary(&session);
        let working_dir = session.working_dir.clone();
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id,
            endpoint: Some(test_endpoint()),
            session,
            working_dir,
            state,
            created_at: 0,
            summary,
            buffered_events: Vec::new(),
            pending_session_change: None,
            pending_request: None,
        })
    }

    #[cfg(test)]
    pub fn background_current_for_test(&mut self) -> Result<usize, BgError> {
        let runtime_id = self.allocate_runtime_id();
        self.background_current(
            test_endpoint(),
            Session::default_session(self.foreground.working_dir.clone()),
            self.foreground.working_dir.clone(),
            runtime_id,
            RuntimeState::Idle,
        )
    }

    #[cfg(test)]
    pub fn resume_for_test(&mut self, slot: usize) -> Result<Session, BgError> {
        Ok(self.resume_slot(slot, RuntimeState::Idle)?.resumed_session)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BgCommand {
    BackgroundCurrent,
    Help,
    List,
    Resume(usize),
    Drop(usize),
}

pub fn parse_bg_command(arg: &str) -> BgCommand {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return BgCommand::BackgroundCurrent;
    }
    if matches!(trimmed, "help" | "-h" | "--help") {
        return BgCommand::Help;
    }
    if matches!(trimmed, "list" | "ls") {
        return BgCommand::List;
    }
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() == 2 && parts[0] == "drop" {
        return parts[1]
            .parse::<usize>()
            .map(BgCommand::Drop)
            .unwrap_or(BgCommand::Help);
    }
    trimmed
        .parse::<usize>()
        .map(BgCommand::Resume)
        .unwrap_or(BgCommand::Help)
}

pub fn render_bg_help() -> String {
    t(Msg::BgHelp).into_owned()
}

pub fn render_bg_list(slots: &BackgroundSlots) -> String {
    if slots.is_empty() {
        return t(Msg::BgListEmpty).into_owned();
    }
    let mut out = t(Msg::BgListHeader).into_owned();
    for row in slots.list_rows() {
        out.push_str(
            &t(Msg::BgListRow {
                slot: row.slot,
                short_id: &row.short_id,
                state: &row.state.localised(),
                age: &humanize_age(row.created_at),
                summary: &row.summary,
            })
            .into_owned(),
        );
    }
    out
}

fn session_summary(session: &Session) -> String {
    if session.name.trim().is_empty() {
        session.short_id().to_string()
    } else {
        session.name.clone()
    }
}

fn retain_session_replay_events(events: &mut Vec<RuntimeEventPayload>) {
    events.retain(|event| {
        matches!(
            event,
            RuntimeEventPayload::Native(CodingRuntimeEvent::SessionNameSuggested { .. })
        )
    });
}

fn background_state_for_stop_reason(reason: atomcode_kernel::event::StopReason) -> RuntimeState {
    match reason {
        atomcode_kernel::event::StopReason::Stopped => RuntimeState::Done,
        atomcode_kernel::event::StopReason::Cancelled => RuntimeState::Cancelled,
        _ => RuntimeState::Error,
    }
}

fn current_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn humanize_age(ts: u64) -> String {
    let now = current_timestamp();
    let d = now.saturating_sub(ts);
    if d < 60 {
        t(Msg::BgAgeNow).into_owned()
    } else if d < 3600 {
        t(Msg::BgAgeMinutes { n: d / 60 }).into_owned()
    } else if d < 86400 {
        t(Msg::BgAgeHours { n: d / 3600 }).into_owned()
    } else {
        t(Msg::BgAgeDays { n: d / 86400 }).into_owned()
    }
}

#[cfg(test)]
fn test_endpoint() -> RuntimeEndpoint {
    let (native, _controls) = atomcode_coding::runtime::coding_runtime_control_channel();
    RuntimeEndpoint {
        native: native.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    use std::path::PathBuf;

    fn session(name: &str) -> Session {
        let mut s = Session::default_session(PathBuf::from("/tmp/project"));
        s.name = name.to_string();
        s
    }

    #[test]
    fn slot_limit_rejects_seventeenth_slot() {
        let mut slots = BackgroundSlots::new(16);
        for i in 0..16 {
            slots
                .push_test_slot(session(&format!("slot-{i}")), RuntimeState::Idle)
                .unwrap();
        }

        let err = slots
            .push_test_slot(session("overflow"), RuntimeState::Idle)
            .unwrap_err();

        assert_eq!(err, BgError::SlotLimit { max: 16 });
    }

    #[test]
    fn drop_compacts_slot_numbers() {
        let mut slots = BackgroundSlots::new(16);
        slots
            .push_test_slot(session("one"), RuntimeState::Idle)
            .unwrap();
        slots
            .push_test_slot(session("two"), RuntimeState::Idle)
            .unwrap();
        slots
            .push_test_slot(session("three"), RuntimeState::Idle)
            .unwrap();

        let dropped = slots.drop_slot(2).unwrap();

        assert_eq!(dropped.session.name, "two");
        assert_eq!(slots.list_rows()[0].slot, 1);
        assert_eq!(slots.list_rows()[1].slot, 2);
        assert_eq!(slots.list_rows()[1].summary, "three");
    }

    #[test]
    fn render_empty_bg_list_mentions_no_background_sessions() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(atomcode_config::locale::Locale::En);
        let slots = BackgroundSlots::new(16);
        assert_eq!(render_bg_list(&slots), "  No background sessions.\n");
    }

    #[test]
    fn a_new_background_turn_drops_the_previous_prompt_suggestion() {
        let mut events = vec![
            RuntimeEventPayload::Native(CodingRuntimeEvent::SessionNameSuggested {
                name: "kept title".into(),
            }),
            RuntimeEventPayload::Native(CodingRuntimeEvent::NextPromptSuggested {
                generation: atomcode_coding::RuntimeGeneration(1),
                session_id: Some("session-1".into()),
                turn_id: 7,
                text: "stale suggestion".into(),
            }),
        ];

        retain_session_replay_events(&mut events);

        assert!(matches!(
            events.as_slice(),
            [RuntimeEventPayload::Native(
                CodingRuntimeEvent::SessionNameSuggested { name }
            )] if name == "kept title"
        ));
    }

    #[test]
    fn background_current_replaces_foreground_and_adds_slot() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager.foreground_session_mut().name = "active task".to_string();

        let slot = manager.background_current_for_test().unwrap();

        assert_eq!(slot, 1);
        assert_eq!(manager.backgrounds().len(), 1);
        assert_eq!(manager.backgrounds().list_rows()[0].summary, "active task");
        assert_eq!(manager.foreground_session().name, "default");
    }

    #[test]
    fn background_current_preserves_foreground_when_slots_are_full() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager.foreground_session_mut().name = "active task".to_string();
        for i in 0..MAX_BACKGROUND_SLOTS {
            let mut session = Session::default_session(PathBuf::from("/tmp/project"));
            session.name = format!("slot {i}");
            manager
                .push_test_background(session, RuntimeState::Idle)
                .unwrap();
        }

        let err = manager.background_current_for_test().unwrap_err();

        assert_eq!(
            err,
            BgError::SlotLimit {
                max: MAX_BACKGROUND_SLOTS
            }
        );
        assert_eq!(manager.backgrounds().len(), MAX_BACKGROUND_SLOTS);
        assert_eq!(manager.foreground_session().name, "active task");
    }

    #[test]
    fn background_turn_complete_updates_slot_to_done_and_messages() {
        use crate::event_loop::ui_event::UiTurnStopReason as TurnStopReason;
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let mut slots = BackgroundSlots::new(16);
        let mut session = Session::default_session(PathBuf::from("/tmp/project"));
        session.name = "task".to_string();
        slots
            .push_test_slot(session, RuntimeState::Running)
            .unwrap();

        slots.apply_event_to_slot(
            1,
            &AgentEvent::TurnComplete {
                duration: std::time::Duration::from_secs(1),
                total_tokens: 10,
                turn_count: 1,
                tool_call_count: 0,
                stop_reason: TurnStopReason::Natural,
                snapshot: SessionSnapshot::new(vec![Message::user("task")]),
            },
        );

        assert_eq!(slots.list_rows()[0].state, RuntimeState::Done);
    }

    #[test]
    fn legacy_background_turn_complete_respects_abnormal_stop_reason() {
        use crate::event_loop::ui_event::UiTurnStopReason as TurnStopReason;

        let mut slots = BackgroundSlots::new(16);
        slots
            .push_test_slot(
                Session::default_session(PathBuf::from("/tmp/project")),
                RuntimeState::Running,
            )
            .unwrap();

        slots.apply_event_to_slot(
            1,
            &AgentEvent::TurnComplete {
                duration: std::time::Duration::from_secs(1),
                total_tokens: 10,
                turn_count: 1,
                tool_call_count: 0,
                stop_reason: TurnStopReason::TurnLimit,
                snapshot: atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
            },
        );

        assert_eq!(slots.list_rows()[0].state, RuntimeState::Error);
    }

    #[test]
    fn legacy_background_error_is_diagnostic_until_terminal() {
        let mut slots = BackgroundSlots::new(16);
        slots
            .push_test_slot(
                Session::default_session(PathBuf::from("/tmp/project")),
                RuntimeState::Running,
            )
            .unwrap();

        slots.apply_event_to_slot(
            1,
            &AgentEvent::Error {
                error: "provider diagnostic".into(),
                snapshot: atomcode_kernel::message::SessionSnapshot::new(Vec::new()),
            },
        );

        assert_eq!(slots.list_rows()[0].state, RuntimeState::Running);
    }

    #[test]
    fn sequenced_native_turn_terminal_updates_background_snapshot_and_state() {
        use std::sync::Arc;

        use atomcode_coding::{RuntimeTurnStats, SequencedRuntimeEvent, TurnCompletion};
        use atomcode_kernel::{
            event::StopReason,
            message::{Message, SessionSnapshot},
        };

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("native task"), RuntimeState::Running)
            .unwrap();
        let snapshot = Arc::new(SessionSnapshot::new(vec![Message::user("native result")]));

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::SequencedNative(SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    snapshot,
                    stats: RuntimeTurnStats::default(),
                }),
            }),
        );

        let slot = &manager.backgrounds.slots[0];
        assert_eq!(slot.state, RuntimeState::Done);
        assert_eq!(slot.session.messages.len(), 1);
        assert_eq!(slot.session.messages[0].text.as_str(), "native result");
    }

    #[test]
    fn sequenced_native_request_is_retained_for_foreground_resume() {
        use std::sync::Arc;

        use atomcode_coding::{RuntimeRequest, SequencedRuntimeEvent};
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("approval task"), RuntimeState::Running)
            .unwrap();
        let request = RuntimeRequest {
            id: 42,
            kind: atomcode_capabilities::tools::APPROVAL_KIND.into(),
            payload: serde_json::json!({
                "call_id": "call-42",
                "tool": "bash",
                "args": "{\"command\":\"pwd\"}"
            }),
            snapshot: Some(Arc::new(SessionSnapshot::new(vec![Message::user(
                "approve this",
            )]))),
        };

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::SequencedNative(SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Request(request.clone()),
            }),
        );
        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert_eq!(resumed.resumed_state, RuntimeState::Running);
        assert!(resumed.replay_events.iter().any(|event| {
            matches!(
                event,
                RuntimeEventPayload::Native(CodingRuntimeEvent::Request(request))
                    if request.id == 42
            )
        }));
        assert_eq!(resumed.resumed_session.messages.len(), 1);
        assert_eq!(
            resumed.resumed_session.messages[0].text.as_str(),
            "approve this"
        );
    }

    #[test]
    fn sequenced_native_agent_output_is_retained_for_foreground_resume() {
        use atomcode_coding::SequencedRuntimeEvent;

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("streaming task"), RuntimeState::Running)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::SequencedNative(SequencedRuntimeEvent {
                generation: 1,
                sequence: 1,
                event: CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::TextDelta(
                    "partial answer".into(),
                )),
            }),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();
        assert!(matches!(
            resumed.replay_events.as_slice(),
            [RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::TextDelta(text)
            ))] if text == "partial answer"
        ));
    }

    #[test]
    fn background_request_replays_after_prior_agent_output() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("approval task"), RuntimeState::Running)
            .unwrap();
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::TextDelta("before approval".into()),
            )),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Request(
                atomcode_coding::RuntimeRequest {
                    id: 7,
                    kind: atomcode_capabilities::tools::APPROVAL_KIND.into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                },
            )),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert!(matches!(
            resumed.replay_events.as_slice(),
            [
                RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                    atomcode_kernel::event::AgentEvent::TextDelta(text)
                )),
                RuntimeEventPayload::Native(CodingRuntimeEvent::Request(request)),
            ] if text == "before approval" && request.id == 7
        ));
    }

    #[test]
    fn request_snapshot_supersedes_already_buffered_turn_output() {
        use std::sync::Arc;

        use atomcode_kernel::message::{Message, SessionSnapshot};

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("approval task"), RuntimeState::Running)
            .unwrap();
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Ui(AgentEvent::UserEcho("question".into())),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::TextDelta("streamed answer".into()),
            )),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Request(
                atomcode_coding::RuntimeRequest {
                    id: 7,
                    kind: atomcode_capabilities::tools::APPROVAL_KIND.into(),
                    payload: serde_json::json!({}),
                    snapshot: Some(Arc::new(SessionSnapshot::new(vec![Message::user(
                        "question",
                    )]))),
                },
            )),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert!(matches!(
            resumed.replay_events.as_slice(),
            [RuntimeEventPayload::Native(CodingRuntimeEvent::Request(request))]
                if request.id == 7
        ));
        assert_eq!(resumed.resumed_session.messages.len(), 1);
    }

    #[test]
    fn shared_resolution_prevents_background_request_from_reappearing_on_resume() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("interactive task"), RuntimeState::Running)
            .unwrap();
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Request(
                atomcode_coding::RuntimeRequest {
                    id: 42,
                    kind: atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND
                        .into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                },
            )),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Ui(AgentEvent::SharedRequestResolved {
                request_id: 42,
                kind: atomcode_capabilities::tools::request_user_input::REQUEST_USER_INPUT_KIND
                    .into(),
            }),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();
        assert!(resumed.replay_events.iter().all(|event| !matches!(
            event,
            RuntimeEventPayload::Native(CodingRuntimeEvent::Request(request))
                if request.id == 42
        )));
    }

    #[test]
    fn background_session_name_is_persisted_before_updating_the_mirror() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("temporary"), RuntimeState::Running)
            .unwrap();
        let expected_id = manager.backgrounds.slots[0].session.id.clone();

        manager.apply_background_session_name_with(
            RuntimeId::new(2),
            "generated title".into(),
            |working_dir, session_id, name| {
                assert_eq!(working_dir, std::path::Path::new("/tmp/project"));
                assert_eq!(session_id, expected_id);
                assert_eq!(name, "generated title");
                Ok(true)
            },
        );

        let background = &manager.backgrounds.slots[0];
        assert_eq!(background.session.name, "generated title");
        assert!(background.session.ai_named);
        assert_eq!(background.summary, "generated title");
    }

    #[test]
    fn failed_background_name_persistence_is_retried_on_resume() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("temporary"), RuntimeState::Done)
            .unwrap();

        manager.apply_background_session_name_with(
            RuntimeId::new(2),
            "generated title".into(),
            |_, _, _| Err("catalog unavailable".into()),
        );
        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert!(matches!(
            resumed.replay_events.as_slice(),
            [RuntimeEventPayload::Native(
                CodingRuntimeEvent::SessionNameSuggested { name }
            )] if name == "generated title"
        ));
    }

    #[test]
    fn background_agent_error_waits_for_authoritative_runtime_terminal() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("startup failure"), RuntimeState::Running)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::Error {
                    message: "provider could not start".into(),
                    http_status: None,
                    code: None,
                },
            )),
        );

        assert_eq!(manager.backgrounds.slots[0].state, RuntimeState::Running);

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::RuntimeStopped(
                atomcode_coding::RuntimeExit {
                    reason: atomcode_coding::RuntimeExitReason::OwnerStopped,
                    forced: false,
                },
            )),
        );
        assert_eq!(manager.backgrounds.slots[0].state, RuntimeState::Error);
    }

    #[test]
    fn abnormal_native_turn_terminals_do_not_look_successful() {
        use std::sync::Arc;

        use atomcode_coding::{RuntimeTurnStats, TurnCompletion};
        use atomcode_kernel::{event::StopReason, message::SessionSnapshot};

        for reason in [
            StopReason::MaxRounds,
            StopReason::MaxContinuations,
            StopReason::RepeatLoop,
            StopReason::ToolLoopDetected,
            StopReason::ProviderError,
            StopReason::Timeout,
            StopReason::PromptRejected,
            StopReason::PolicyDenied,
            StopReason::RateLimited,
        ] {
            let mut manager = BgRuntimeManager::new_for_test(Session::default_session(
                PathBuf::from("/tmp/project"),
            ));
            manager
                .push_test_background(session("failed task"), RuntimeState::Running)
                .unwrap();
            manager.apply_background_event(
                RuntimeId::new(2),
                RuntimeEventPayload::Native(CodingRuntimeEvent::TurnFinished(
                    TurnCompletion::Completed {
                        turn_id: 1,
                        reason,
                        snapshot: Arc::new(SessionSnapshot::new(Vec::new())),
                        stats: RuntimeTurnStats::default(),
                    },
                )),
            );

            assert_eq!(manager.backgrounds.slots[0].state, RuntimeState::Error);
        }
    }

    #[test]
    fn policy_intervention_and_terminal_survive_background_resume_together() {
        use std::sync::Arc;

        use atomcode_coding::{RuntimeTurnStats, TurnCompletion};
        use atomcode_kernel::{event::StopReason, message::SessionSnapshot};

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("guarded task"), RuntimeState::Running)
            .unwrap();
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::PolicyIntervention {
                    intervention:
                        atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(),
                },
            )),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::PolicyDenied,
                    snapshot: Arc::new(SessionSnapshot::new(Vec::new())),
                    stats: RuntimeTurnStats::default(),
                },
            )),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();
        assert_eq!(resumed.resumed_state, RuntimeState::Error);
        assert!(matches!(
            resumed.replay_events.as_slice(),
            [
                RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                    atomcode_kernel::event::AgentEvent::PolicyIntervention { .. }
                )),
                RuntimeEventPayload::Native(CodingRuntimeEvent::TurnFinished(
                    TurnCompletion::Completed {
                        reason: StopReason::PolicyDenied,
                        ..
                    }
                ))
            ]
        ));
    }

    #[test]
    fn snapshot_failure_keeps_partial_output_available_for_resume() {
        use atomcode_coding::{RuntimeSnapshotError, RuntimeTurnStats, TurnCompletion};
        use atomcode_kernel::event::StopReason;

        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        manager
            .push_test_background(session("failed snapshot"), RuntimeState::Running)
            .unwrap();
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::TextDelta("partial answer".into()),
            )),
        );
        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::TurnFinished(
                TurnCompletion::SnapshotUnavailable {
                    turn_id: 1,
                    reason: StopReason::ProviderError,
                    error: RuntimeSnapshotError {
                        message: "snapshot failed".into(),
                    },
                    stats: RuntimeTurnStats::default(),
                },
            )),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert_eq!(resumed.resumed_state, RuntimeState::Error);
        assert!(matches!(
            resumed.replay_events.first(),
            Some(RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::TextDelta(text)
            ))) if text == "partial answer"
        ));
        assert!(matches!(
            resumed.replay_events.last(),
            Some(RuntimeEventPayload::Native(CodingRuntimeEvent::Agent(
                atomcode_kernel::event::AgentEvent::Error { message, .. }
            ))) if message == "snapshot failed"
        ));
    }

    #[test]
    fn resume_slot_discards_empty_foreground() {
        let mut manager =
            BgRuntimeManager::new_for_test(Session::default_session(PathBuf::from("/tmp/project")));
        let mut bg_session = Session::default_session(PathBuf::from("/tmp/project"));
        bg_session.name = "background task".to_string();
        manager
            .push_test_background(bg_session, RuntimeState::Done)
            .unwrap();

        let resumed = manager.resume_for_test(1).unwrap();

        assert_eq!(resumed.name, "background task");
        assert_eq!(manager.backgrounds().len(), 0);
    }

    #[tokio::test]
    async fn resume_restores_the_native_handle_for_that_runtime() {
        use atomcode_coding::runtime::{coding_runtime_control_channel, CodingRuntimeControl};

        let (first_native, mut first_controls) = coding_runtime_control_channel();
        let first_endpoint = RuntimeEndpoint {
            native: first_native.into(),
        };
        let first = session("first");
        let mut manager = BgRuntimeManager::new(
            first.clone(),
            first.working_dir.clone(),
            RuntimeId::new(1),
            first_endpoint,
        );

        let (second_native, _second_controls) = coding_runtime_control_channel();
        manager
            .background_current(
                RuntimeEndpoint {
                    native: second_native.into(),
                },
                session("second"),
                PathBuf::from("/tmp/project"),
                RuntimeId::new(2),
                RuntimeState::Idle,
            )
            .unwrap();

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();
        resumed
            .resumed_endpoint
            .native
            .compact(Some("first runtime".into()))
            .unwrap();

        assert!(matches!(
            first_controls.recv().await,
            Some(CodingRuntimeControl::Compact { focus: Some(focus), .. })
                if focus == "first runtime"
        ));
    }

    #[test]
    fn resume_restores_the_runtime_working_dir_instead_of_stale_session_metadata() {
        let first_runtime_dir = PathBuf::from("/projects/first");
        let second_runtime_dir = PathBuf::from("/projects/second");
        let mut first = Session::default_session(PathBuf::from("/stale/first"));
        first.name = "first".into();
        let mut second = Session::default_session(PathBuf::from("/stale/second"));
        second.name = "second".into();
        let mut manager = BgRuntimeManager::new_for_test_at(first, first_runtime_dir.clone());

        manager
            .background_current(
                test_endpoint(),
                second,
                second_runtime_dir.clone(),
                RuntimeId::new(2),
                RuntimeState::Idle,
            )
            .unwrap();

        let resumed = manager.resume_slot(1, RuntimeState::Running).unwrap();

        assert_eq!(resumed.resumed_working_dir, first_runtime_dir);
        assert_eq!(
            manager.foreground_working_dir(),
            &resumed.resumed_working_dir
        );
        assert_eq!(manager.backgrounds.slots[0].working_dir, second_runtime_dir);
    }

    #[test]
    fn resume_uses_working_dir_changed_while_runtime_was_backgrounded() {
        let first_runtime_dir = PathBuf::from("/projects/first");
        let changed_runtime_dir = PathBuf::from("/projects/first/nested");
        let mut manager = BgRuntimeManager::new_for_test_at(
            Session::default_session(first_runtime_dir.clone()),
            first_runtime_dir,
        );

        manager.background_current_for_test().unwrap();
        manager.apply_background_event(
            RuntimeId::new(1),
            RuntimeEventPayload::Native(CodingRuntimeEvent::WorkingDirectoryChanged(
                changed_runtime_dir.clone(),
            )),
        );

        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert_eq!(resumed.resumed_working_dir, changed_runtime_dir);
    }

    #[test]
    fn resume_without_runtime_endpoint_leaves_manager_unchanged() {
        let first_dir = PathBuf::from("/projects/first");
        let mut manager = BgRuntimeManager::new_for_test_at(
            Session::default_session(first_dir.clone()),
            first_dir,
        );
        manager.background_current_for_test().unwrap();
        manager.backgrounds.slots[0].endpoint = None;
        let foreground_runtime_id = manager.foreground.runtime_id;
        let foreground_session_id = manager.foreground.session.id.clone();
        let foreground_working_dir = manager.foreground.working_dir.clone();
        let background_runtime_id = manager.backgrounds.slots[0].runtime_id;
        let background_session_id = manager.backgrounds.slots[0].session.id.clone();

        let error = match manager.resume_slot(1, RuntimeState::Idle) {
            Err(error) => error,
            Ok(_) => panic!("resume without an endpoint must fail"),
        };

        assert_eq!(error, BgError::NoRuntimeClient { slot: 1 });
        assert_eq!(manager.foreground.runtime_id, foreground_runtime_id);
        assert_eq!(manager.foreground.session.id, foreground_session_id);
        assert_eq!(manager.foreground.working_dir, foreground_working_dir);
        assert_eq!(manager.backgrounds.len(), 1);
        assert_eq!(
            manager.backgrounds.slots[0].runtime_id,
            background_runtime_id
        );
        assert_eq!(
            manager.backgrounds.slots[0].session.id,
            background_session_id
        );
    }

    #[test]
    fn background_session_changed_replaces_the_complete_session_projection() {
        let original_dir = PathBuf::from("/projects/original");
        let changed_dir = PathBuf::from("/projects/changed");
        let mut manager = BgRuntimeManager::new_for_test_at(
            Session::default_session(original_dir.clone()),
            original_dir,
        );
        manager.background_current_for_test().unwrap();
        let runtime_id = manager.backgrounds.slots[0].runtime_id;
        let changed = atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(2),
            session_id: Some("changed-session".into()),
            working_dir: changed_dir.clone(),
        };
        let mut loaded = Session::default_session(PathBuf::from("/stale/catalog/path"));
        loaded.id = "changed-session".into();
        loaded.name = "loaded from exact bucket".into();

        manager.apply_background_session_changed_with(runtime_id, changed, |_| Ok(loaded.clone()));
        let resumed = manager.resume_slot(1, RuntimeState::Idle).unwrap();

        assert_eq!(resumed.resumed_session.id, "changed-session");
        assert_eq!(resumed.resumed_session.name, "loaded from exact bucket");
        assert_eq!(resumed.resumed_working_dir, changed_dir);
    }

    #[test]
    fn background_session_loader_uses_the_exact_project_bucket() {
        use atomcode_capabilities::session::{
            PresentationFile, SessionManager, SessionMeta, StorageOwner,
        };
        use atomcode_kernel::message::{Message, SessionSnapshot};

        let root = tempfile::tempdir().unwrap();
        let id = "same-session-id";
        let first_dir = PathBuf::from("/projects/first");
        let second_dir = PathBuf::from("/projects/second");
        for (working_dir, name) in [(&first_dir, "first"), (&second_dir, "second")] {
            let bucket = SessionManager::project_hash(working_dir);
            let manager = SessionManager::with_root(root.path().join(bucket));
            let lease = manager.acquire_lease(id).unwrap();
            let snapshot = SessionSnapshot::new(vec![Message::user(name)]);
            let mut meta = SessionMeta::new(id, working_dir.to_string_lossy(), 1);
            meta.owner = StorageOwner::Native;
            meta.name = name.into();
            meta.message_count = 1;
            manager
                .commit_native_import(
                    &lease,
                    Some(&snapshot),
                    Some(&PresentationFile::default()),
                    &meta,
                )
                .unwrap();
        }
        let changed = atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(2),
            session_id: Some(id.into()),
            working_dir: second_dir,
        };

        let loaded = load_background_session_projection_in_root(root.path(), &changed).unwrap();

        assert_eq!(loaded.id, id);
        assert_eq!(loaded.name, "second");
        assert_eq!(loaded.messages[0].text.as_str(), "second");
    }

    #[test]
    fn unresolved_background_session_change_blocks_resume_without_mutation() {
        let original_dir = PathBuf::from("/projects/original");
        let changed_dir = PathBuf::from("/projects/changed");
        let mut manager = BgRuntimeManager::new_for_test_at(
            Session::default_session(original_dir.clone()),
            original_dir,
        );
        manager.background_current_for_test().unwrap();
        let runtime_id = manager.backgrounds.slots[0].runtime_id;
        let changed = atomcode_coding::SessionChanged {
            generation: atomcode_coding::RuntimeGeneration(2),
            session_id: Some("missing-session".into()),
            working_dir: changed_dir,
        };
        manager.apply_background_session_changed_with(runtime_id, changed, |_| {
            Err("catalog read failed".into())
        });
        let foreground_runtime_id = manager.foreground.runtime_id;
        let background_session_id = manager.backgrounds.slots[0].session.id.clone();

        let error = match manager.resume_slot_with_loader(1, RuntimeState::Idle, |_| {
            Err("catalog still unavailable".into())
        }) {
            Err(error) => error,
            Ok(_) => panic!("resume with an unresolved session projection must fail"),
        };

        assert!(matches!(
            error,
            BgError::SessionProjectionUnavailable { slot: 1, .. }
        ));
        assert_eq!(manager.foreground.runtime_id, foreground_runtime_id);
        assert_eq!(manager.backgrounds.len(), 1);
        assert_eq!(
            manager.backgrounds.slots[0].session.id,
            background_session_id
        );
    }

    #[test]
    fn parse_bg_subcommands_use_bare_names() {
        assert_eq!(parse_bg_command("list"), BgCommand::List);
        assert_eq!(parse_bg_command("drop 2"), BgCommand::Drop(2));
        assert_eq!(parse_bg_command("help"), BgCommand::Help);
    }

    #[test]
    fn parse_bg_rejects_nested_slash_subcommands() {
        assert_eq!(parse_bg_command("/list"), BgCommand::Help);
        assert_eq!(parse_bg_command("/drop 2"), BgCommand::Help);
    }

    #[test]
    fn parse_bg_numeric_resumes_slot() {
        assert_eq!(parse_bg_command("3"), BgCommand::Resume(3));
    }

    #[tokio::test]
    async fn runtime_event_forwarder_tags_events() {
        use crate::event_loop::ui_event::UiEvent as AgentEvent;

        let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fan_tx, mut fan_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime_id = RuntimeId::new(7);

        spawn_event_forwarder(runtime_id, agent_rx, fan_tx);
        agent_tx
            .send(RuntimeEventPayload::Ui(AgentEvent::TextDelta(
                "hello".to_string(),
            )))
            .unwrap();

        let event = fan_rx.recv().await.unwrap();
        assert_eq!(event.runtime_id, runtime_id);
        assert!(matches!(
            event.event,
            RuntimeEventPayload::Ui(AgentEvent::TextDelta(text)) if text == "hello"
        ));
    }

    #[test]
    fn manual_compaction_finish_releases_background_runtime() {
        use atomcode_coding::runtime::CompactionOutcome;
        use atomcode_kernel::message::CompactTrigger;

        let project = PathBuf::from("/tmp/project");
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        manager
            .push_test_background(session("compacting"), RuntimeState::Running)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(CompactionOutcome {
                    trigger: CompactTrigger::Manual { focus: None },
                    epoch: 1,
                    removed_messages: 0,
                    bytes_before: 0,
                    bytes_after: 0,
                    committed: false,
                    estimated_tokens_before: 0,
                    estimated_tokens_after: 0,
                    committed_snapshot: None,
                }),
            }),
        );

        assert_eq!(
            manager.backgrounds().list_rows()[0].state,
            RuntimeState::Idle
        );
    }

    #[test]
    #[serial_test::serial]
    fn committed_manual_compaction_updates_background_session_mirror() {
        use atomcode_coding::runtime::CompactionOutcome;
        use atomcode_kernel::message::{CompactTrigger, Message, SessionSnapshot};

        let project = tempfile::tempdir().unwrap();
        let project = project.path().to_path_buf();
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        manager
            .push_test_background(session("compacting"), RuntimeState::Running)
            .unwrap();
        let snapshot = SessionSnapshot::new(vec![Message::user("after compact")]);

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(CompactionOutcome {
                    trigger: CompactTrigger::Manual { focus: None },
                    epoch: 1,
                    removed_messages: 2,
                    bytes_before: 100,
                    bytes_after: 50,
                    committed: true,
                    estimated_tokens_before: 25,
                    estimated_tokens_after: 12,
                    committed_snapshot: Some(std::sync::Arc::new(snapshot)),
                }),
            }),
        );

        let messages = &manager.backgrounds.slots[0].session.messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text.as_str(), "after compact");
        assert_eq!(
            manager.backgrounds().list_rows()[0].state,
            RuntimeState::Idle
        );
    }

    #[test]
    fn manual_compaction_interruption_releases_background_runtime() {
        use atomcode_kernel::message::CompactTrigger;

        let project = PathBuf::from("/tmp/project");
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        manager
            .push_test_background(session("compacting"), RuntimeState::Running)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Interrupted {
                    trigger: CompactTrigger::Manual { focus: None },
                    reason: atomcode_coding::runtime::CompactionInterruption::RuntimeReconfigured,
                },
            }),
        );

        assert_eq!(
            manager.backgrounds().list_rows()[0].state,
            RuntimeState::Idle
        );
    }

    #[test]
    fn failed_manual_compaction_marks_background_runtime_error() {
        use atomcode_kernel::message::CompactTrigger;

        let project = PathBuf::from("/tmp/project");
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        manager
            .push_test_background(session("compacting"), RuntimeState::Running)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Failed {
                    trigger: CompactTrigger::Manual { focus: None },
                    error: atomcode_kernel::checkpoint::CompactionCheckpointError::new("disk full"),
                },
            }),
        );

        assert_eq!(
            manager.backgrounds().list_rows()[0].state,
            RuntimeState::Error
        );
    }

    #[test]
    fn late_compaction_finish_does_not_downgrade_terminal_background_state() {
        use atomcode_coding::runtime::CompactionOutcome;
        use atomcode_kernel::message::CompactTrigger;

        let project = PathBuf::from("/tmp/project");
        let mut manager = BgRuntimeManager::new_for_test(Session::default_session(project.clone()));
        manager
            .push_test_background(session("completed"), RuntimeState::Done)
            .unwrap();

        manager.apply_background_event(
            RuntimeId::new(2),
            RuntimeEventPayload::Native(CodingRuntimeEvent::CompactionFinished {
                completion: CompactionCompletion::Completed(CompactionOutcome {
                    trigger: CompactTrigger::Manual { focus: None },
                    epoch: 1,
                    removed_messages: 2,
                    bytes_before: 100,
                    bytes_after: 50,
                    committed: true,
                    estimated_tokens_before: 25,
                    estimated_tokens_after: 12,
                    committed_snapshot: None,
                }),
            }),
        );

        assert_eq!(
            manager.backgrounds().list_rows()[0].state,
            RuntimeState::Done
        );
    }
}
