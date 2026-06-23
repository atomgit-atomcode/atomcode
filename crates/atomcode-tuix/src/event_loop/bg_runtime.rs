use atomcode_core::agent::{AgentClient, AgentEvent};
use atomcode_core::i18n::{t, Msg};
use atomcode_core::session::{Session, SessionManager};

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
    pub event: AgentEvent,
}

pub fn spawn_event_forwarder(
    runtime_id: RuntimeId,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
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
}

pub struct ForegroundRuntime {
    pub runtime_id: RuntimeId,
    pub client: Option<AgentClient>,
    pub session: Session,
}

pub struct BackgroundSlot {
    pub runtime_id: RuntimeId,
    pub client: Option<AgentClient>,
    pub session: Session,
    pub state: RuntimeState,
    pub created_at: u64,
    pub summary: String,
    pub buffered_events: Vec<AgentEvent>,
}

impl BackgroundSlot {
    fn into_foreground(self) -> ForegroundRuntime {
        ForegroundRuntime {
            runtime_id: self.runtime_id,
            client: self.client,
            session: self.session,
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
            AgentEvent::TurnComplete { snapshot, .. } => {
                bg.state = RuntimeState::Done;
                super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                bg.summary = session_summary(&bg.session);
                true
            }
            AgentEvent::TurnCancelled { snapshot } => {
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
                }
                false
            }
            AgentEvent::Error { snapshot, .. } => {
                // Persist whatever conversation state we have at error
                // time so a background session that died mid-turn can
                // still be `/resume`d to inspect the partial transcript.
                // Empty snapshots from the streaming-error forwarder
                // skips the snapshot (no fresh state to commit). Return
                // `true` so `apply_background_event` flushes to disk.
                let did_snapshot = !snapshot.messages.is_empty();
                if did_snapshot {
                    super::apply_session_snapshot(&mut bg.session, snapshot.clone());
                    bg.summary = session_summary(&bg.session);
                }
                bg.state = RuntimeState::Error;
                did_snapshot
            }
            AgentEvent::TextDelta(_)
            | AgentEvent::ReasoningDelta(_)
            | AgentEvent::ToolCallStreaming { .. }
            | AgentEvent::ToolCallStarted { .. }
            | AgentEvent::ToolOutputChunk { .. }
            | AgentEvent::ToolCallResult { .. } => {
                bg.buffered_events.push(event.clone());
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
            client: None,
            summary: session.name.clone(),
            session,
            state,
            created_at: 0,
            buffered_events: Vec::new(),
        })
    }
}

pub struct ResumeOutcome {
    pub resumed_session: Session,
    pub resumed_runtime_id: RuntimeId,
    pub resumed_client: Option<AgentClient>,
    pub previous_foreground_slot: Option<usize>,
}

pub struct BgRuntimeManager {
    foreground: ForegroundRuntime,
    backgrounds: BackgroundSlots,
    next_runtime_id: u64,
}

impl BgRuntimeManager {
    pub fn new(session: Session, runtime_id: RuntimeId, client: AgentClient) -> Self {
        Self {
            foreground: ForegroundRuntime {
                runtime_id,
                client: Some(client),
                session,
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

    pub fn set_foreground_session(&mut self, session: Session) {
        self.foreground.session = session;
    }

    pub fn set_foreground_runtime(
        &mut self,
        runtime_id: RuntimeId,
        client: AgentClient,
        session: Session,
    ) {
        self.foreground = ForegroundRuntime {
            runtime_id,
            client: Some(client),
            session,
        };
    }

    pub fn background_current(
        &mut self,
        new_client: AgentClient,
        new_session: Session,
        new_runtime_id: RuntimeId,
        current_state: RuntimeState,
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
                client: Some(new_client),
                session: new_session,
            },
        );
        let summary = session_summary(&old.session);
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id: old.runtime_id,
            client: old.client,
            session: old.session,
            state: current_state,
            created_at: current_timestamp(),
            summary,
            buffered_events: Vec::new(),
        })
    }

    pub fn push_background_runtime(
        &mut self,
        runtime_id: RuntimeId,
        client: AgentClient,
        session: Session,
        state: RuntimeState,
    ) -> Result<usize, BgError> {
        let summary = session_summary(&session);
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id,
            client: Some(client),
            session,
            state,
            created_at: current_timestamp(),
            summary,
            buffered_events: Vec::new(),
        })
    }

    pub fn resume_slot(
        &mut self,
        slot: usize,
        current_state: RuntimeState,
    ) -> Result<ResumeOutcome, BgError> {
        let resumed = self.backgrounds.drop_slot(slot)?;
        let old_foreground = std::mem::replace(&mut self.foreground, resumed.into_foreground());
        let old_had_state = !old_foreground.session.messages.is_empty()
            || matches!(current_state, RuntimeState::Running);
        let previous_foreground_slot = if old_had_state {
            let summary = session_summary(&old_foreground.session);
            Some(self.backgrounds.push_slot(BackgroundSlot {
                runtime_id: old_foreground.runtime_id,
                client: old_foreground.client,
                session: old_foreground.session,
                state: current_state,
                created_at: current_timestamp(),
                summary,
                buffered_events: Vec::new(),
            })?)
        } else {
            None
        };

        Ok(ResumeOutcome {
            resumed_session: self.foreground.session.clone(),
            resumed_runtime_id: self.foreground.runtime_id,
            resumed_client: self.foreground.client.clone(),
            previous_foreground_slot,
        })
    }

    pub fn drop_slot(&mut self, slot: usize) -> Result<BackgroundSlot, BgError> {
        self.backgrounds.drop_slot(slot)
    }

    pub fn apply_background_event(
        &mut self,
        runtime_id: RuntimeId,
        event: AgentEvent,
        session_manager: &SessionManager,
    ) {
        let Some(slot) = self.backgrounds.slot_for_runtime_id(runtime_id) else {
            return;
        };
        let terminal = self.backgrounds.apply_event_to_slot(slot, &event);
        if terminal {
            if let Some(bg) = self.backgrounds.slot_mut_for_runtime_id(runtime_id) {
                let _ = session_manager.save(&bg.session);
            }
        }
    }

    #[cfg(test)]
    pub fn new_for_test(session: Session) -> Self {
        Self {
            foreground: ForegroundRuntime {
                runtime_id: RuntimeId::new(1),
                client: None,
                session,
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
    pub fn push_test_background(
        &mut self,
        session: Session,
        state: RuntimeState,
    ) -> Result<usize, BgError> {
        let runtime_id = self.allocate_runtime_id();
        let summary = session_summary(&session);
        self.backgrounds.push_slot(BackgroundSlot {
            runtime_id,
            client: None,
            session,
            state,
            created_at: 0,
            summary,
            buffered_events: Vec::new(),
        })
    }

    #[cfg(test)]
    pub fn background_current_for_test(&mut self) -> Result<usize, BgError> {
        let runtime_id = self.allocate_runtime_id();
        self.background_current(
            test_client(),
            Session::default_session(self.foreground.session.working_dir.clone()),
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
        out.push_str(&t(Msg::BgListRow {
            slot: row.slot,
            short_id: &row.short_id,
            state: &row.state.localised(),
            age: &humanize_age(row.created_at),
            summary: &row.summary,
        }).into_owned());
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
fn test_client() -> AgentClient {
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    AgentClient {
        cmd_tx,
        tool_registry: std::sync::Arc::new(atomcode_core::tool::ToolRegistry::new()),
        skill_registry: std::sync::Arc::new(std::sync::RwLock::new(
            atomcode_core::skill::SkillRegistry::new(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::session::Session;
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
        crate::i18n::set_locale(atomcode_core::locale::Locale::En);
        let slots = BackgroundSlots::new(16);
        assert_eq!(render_bg_list(&slots), "  No background sessions.\n");
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
        use atomcode_core::agent::TurnStopReason;
        use atomcode_core::conversation::{
            message::{Message, Role},
            ConversationSnapshot,
        };

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
                snapshot: ConversationSnapshot {
                    messages: vec![Message::new(Role::User, "task")],
                    cold_summaries: Vec::new(),
                },
            },
        );

        assert_eq!(slots.list_rows()[0].state, RuntimeState::Done);
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
        use atomcode_core::agent::AgentEvent;

        let (agent_tx, agent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fan_tx, mut fan_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime_id = RuntimeId::new(7);

        spawn_event_forwarder(runtime_id, agent_rx, fan_tx);
        agent_tx
            .send(AgentEvent::TextDelta("hello".to_string()))
            .unwrap();

        let event = fan_rx.recv().await.unwrap();
        assert_eq!(event.runtime_id, runtime_id);
        assert!(matches!(event.event, AgentEvent::TextDelta(text) if text == "hello"));
    }
}
