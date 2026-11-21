//! Read-only presentation projection. Never used to resume or mutate an agent.
use super::*;

/// A complete baseline plus every runtime event since it, through `sequence`.
#[derive(Clone, Debug)]
pub struct RuntimeLiveView {
    pub generation: u64,
    pub mode: RuntimeMode,
    /// Display metadata from the lead agent, retained across transcript rebases.
    pub model: Option<String>,
    pub session_id: String,
    pub working_dir: std::path::PathBuf,
    pub snapshot: Arc<SessionSnapshot>,
    pub events: Vec<SequencedRuntimeEvent>,
    pub sequence: Option<u64>,
    pub(crate) valid: bool,
    pending: BTreeMap<RequestId, SequencedRuntimeEvent>,
    goal: Option<SequencedRuntimeEvent>,
    looping: Option<SequencedRuntimeEvent>,
    goal_sampled_at: Option<tokio::time::Instant>,
    loop_sampled_at: Option<tokio::time::Instant>,
    policy: Option<SequencedRuntimeEvent>,
    turn: Option<SequencedRuntimeEvent>,
    provider_pending: Option<SequencedRuntimeEvent>,
}

fn sample_controller(
    event: &mut Option<SequencedRuntimeEvent>,
    at: &mut Option<tokio::time::Instant>,
) {
    let now = tokio::time::Instant::now();
    let elapsed = at
        .map(|at| now.saturating_duration_since(at).as_secs())
        .unwrap_or(0);
    if let Some(event) = event.as_mut() {
        let recovery = match &mut event.event {
            CodingRuntimeEvent::GoalChanged(progress) => progress.recovery.as_mut(),
            CodingRuntimeEvent::LoopChanged(progress) => progress.recovery.as_mut(),
            _ => None,
        };
        if let Some(recovery) = recovery {
            recovery.retry_in_secs = recovery.retry_in_secs.saturating_sub(elapsed);
        }
        *at = Some(now);
    }
}

impl RuntimeLiveView {
    pub(super) fn has_pending_blocking_request(&self) -> bool {
        self.pending.values().any(|event| {
            matches!(&event.event,
            CodingRuntimeEvent::Request(request)
                if !atomcode_capabilities::tools::request_user_input::is_continued_question(
                    &request.kind, &request.payload))
        })
    }

    pub fn from_snapshot(
        generation: u64,
        session_id: String,
        working_dir: std::path::PathBuf,
        snapshot: SessionSnapshot,
    ) -> Self {
        Self {
            generation,
            mode: RuntimeMode::Build,
            model: None,
            session_id,
            working_dir,
            snapshot: Arc::new(snapshot),
            events: Vec::new(),
            sequence: None,
            valid: true,
            pending: BTreeMap::new(),
            goal: None,
            looping: None,
            goal_sampled_at: None,
            loop_sampled_at: None,
            policy: None,
            turn: None,
            provider_pending: None,
        }
    }

    /// A presentation read ages controller countdowns without changing the
    /// runtime or creating new facts. Late WebUI attachment must not restart a wait.
    pub(super) fn sampled(&self) -> Self {
        let mut view = self.clone();
        sample_controller(&mut view.goal, &mut view.goal_sampled_at);
        sample_controller(&mut view.looping, &mut view.loop_sampled_at);
        for event in &mut view.events {
            let latest = match &event.event {
                CodingRuntimeEvent::GoalChanged(_) => view.goal.as_ref(),
                CodingRuntimeEvent::LoopChanged(_) => view.looping.as_ref(),
                _ => None,
            };
            if let Some(latest) = latest.filter(|latest| latest.sequence == event.sequence) {
                *event = latest.clone();
            }
        }
        view
    }

    /// A harness provider patch keeps the same running conversation and requests.
    pub(super) fn carry_generation(&mut self, generation: u64) {
        self.generation = generation;
        self.model = None;
        for event in &mut self.events {
            event.generation = generation;
        }
        for event in self.pending.values_mut() {
            event.generation = generation;
        }
        for event in self
            .goal
            .iter_mut()
            .chain(self.looping.iter_mut())
            .chain(self.policy.iter_mut())
            .chain(self.turn.iter_mut())
            .chain(self.provider_pending.iter_mut())
        {
            event.generation = generation;
        }
    }

    pub(super) fn observe(
        &mut self,
        envelope: &SequencedRuntimeEvent,
        seed: Option<(SessionSnapshot, RuntimeMode, Option<String>)>,
    ) {
        if let CodingRuntimeEvent::SessionChanged(changed) = &envelope.event {
            self.generation = envelope.generation;
            self.session_id = changed.session_id.clone().unwrap_or_default();
            self.working_dir = changed.working_dir.clone();
            self.model = None;
            self.events.clear();
            self.pending.clear();
            self.goal = None;
            self.looping = None;
            self.goal_sampled_at = None;
            self.loop_sampled_at = None;
            self.policy = None;
            self.turn = None;
            self.provider_pending = None;
            self.valid = seed.is_some();
            if let Some((snapshot, mode, model)) = seed {
                self.model = model;
                self.snapshot = Arc::new(snapshot);
                self.mode = mode;
            }
        } else if envelope.generation != self.generation {
            self.valid = false;
        } else {
            if let CodingRuntimeEvent::ModeChanged { mode } = &envelope.event {
                self.mode = *mode;
            }
            match &envelope.event {
                CodingRuntimeEvent::ProviderChanged { model, .. } => {
                    self.model = Some(model.clone())
                }
                CodingRuntimeEvent::Agent(AgentEvent::Described { description })
                    if description.session == self.session_id =>
                {
                    self.model = description.model.clone()
                }
                _ => {}
            }
            // Cards remain small control state even if transcript replay hits
            // its bound. A later complete snapshot can then safely recover.
            match &envelope.event {
                CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. }) => {
                    self.turn = Some(envelope.clone());
                }
                CodingRuntimeEvent::Request(request) => {
                    self.pending.insert(request.id, envelope.clone());
                }
                CodingRuntimeEvent::RequestResolved { id, .. } => {
                    self.pending.remove(id);
                }
                CodingRuntimeEvent::ProviderSelectionPending { current } => {
                    self.provider_pending = current.as_ref().map(|_| envelope.clone());
                }
                CodingRuntimeEvent::GoalChanged(_) => {
                    self.goal = Some(envelope.clone());
                    self.goal_sampled_at = Some(tokio::time::Instant::now());
                }
                CodingRuntimeEvent::LoopChanged(_) => {
                    self.looping = Some(envelope.clone());
                    self.loop_sampled_at = Some(tokio::time::Instant::now());
                }
                CodingRuntimeEvent::Agent(AgentEvent::PolicyIntervention { .. }) => {
                    self.policy = Some(envelope.clone());
                }
                CodingRuntimeEvent::PolicyInterventionResolved { .. }
                | CodingRuntimeEvent::PolicyInterventionCleared { .. } => {
                    self.policy = None;
                }
                _ => {}
            }
            let snapshot = match &envelope.event {
                CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    snapshot, ..
                }) => Some(snapshot.clone()),
                CodingRuntimeEvent::SnapshotRestoreFinished {
                    result: Ok(snapshot),
                    ..
                } => Some(snapshot.clone()),
                CodingRuntimeEvent::UndoFinished(Ok(result)) => Some(result.snapshot.clone()),
                CodingRuntimeEvent::RewindFinished(Ok(result)) => Some(result.snapshot.clone()),
                CodingRuntimeEvent::CompactionFinished {
                    completion: CompactionCompletion::Completed(outcome),
                } if outcome.committed => outcome.committed_snapshot.clone(),
                _ => None,
            };
            if let Some(snapshot) = snapshot {
                self.snapshot = snapshot;
                // A completed turn can leave a goal question open. Keep only
                // unresolved requests and the current goal alongside the new base.
                let completed = matches!(
                    &envelope.event,
                    CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed { .. })
                );
                if completed {
                    self.pending.retain(|_, event| matches!(&event.event, CodingRuntimeEvent::Request(request) if atomcode_capabilities::tools::request_user_input::is_continued_question(&request.kind, &request.payload)));
                }
                self.events = self.pending.values().cloned().collect();
                if matches!(
                    &envelope.event,
                    CodingRuntimeEvent::CompactionFinished { .. }
                ) {
                    // Automatic compaction replaces history inside a running
                    // turn. Keep its start marker, without replaying old text
                    // already represented by the compacted snapshot.
                    self.events.extend(self.turn.clone());
                } else {
                    self.turn = None;
                }
                if completed {
                    self.provider_pending = None;
                }
                self.events.extend(self.provider_pending.clone());
                self.events.extend(self.goal.clone());
                self.events.extend(self.looping.clone());
                if matches!(
                    &envelope.event,
                    CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                        reason: atomcode_kernel::event::StopReason::PolicyDenied,
                        ..
                    })
                ) {
                    self.events.extend(self.policy.clone());
                }
                self.events.sort_by_key(|event| event.sequence);
                self.valid = true;
            } else if matches!(
                &envelope.event,
                CodingRuntimeEvent::RuntimeStopped(_)
                    | CodingRuntimeEvent::TurnFinished(TurnCompletion::SnapshotUnavailable { .. })
            ) {
                self.valid = false;
            } else if self.valid {
                // Token chunks can be numerous in long answers. Coalesce only
                // adjacent chunks of the same kind, preserving their bytes and
                // the last sequence so the host's delayed originals deduplicate.
                let merged = self.events.last_mut().is_some_and(|last| {
                    let chunks = match (&mut last.event, &envelope.event) {
                        (
                            CodingRuntimeEvent::Agent(AgentEvent::TextDelta(a)),
                            CodingRuntimeEvent::Agent(AgentEvent::TextDelta(b)),
                        )
                        | (
                            CodingRuntimeEvent::Agent(AgentEvent::Reasoning(a)),
                            CodingRuntimeEvent::Agent(AgentEvent::Reasoning(b)),
                        ) => Some((a, b)),
                        (
                            CodingRuntimeEvent::Agent(AgentEvent::ToolCallStreaming {
                                index: a_index,
                                id: a_id,
                                name: a_name,
                                arguments: a,
                            }),
                            CodingRuntimeEvent::Agent(AgentEvent::ToolCallStreaming {
                                index: b_index,
                                id: b_id,
                                name: b_name,
                                arguments: b,
                            }),
                        ) if a_index == b_index
                            && (b_id.is_none() || a_id == b_id)
                            && (b_name.is_none() || a_name == b_name) =>
                        {
                            Some((a, b))
                        }
                        _ => None,
                    };
                    if let Some((a, b)) = chunks {
                        a.push_str(b);
                        last.sequence = envelope.sequence;
                        true
                    } else {
                        false
                    }
                });
                if !merged {
                    // Fail closed rather than retain an unlimited second log.
                    if self.events.len() >= 16_384 {
                        self.events.clear();
                        self.valid = false;
                    } else {
                        self.events.push(envelope.clone());
                    }
                }
            }
        }
        self.sequence = Some(envelope.sequence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view() -> RuntimeLiveView {
        RuntimeLiveView::from_snapshot(
            0,
            "old".into(),
            "/old".into(),
            SessionSnapshot::new(vec![Message::user("history")]),
        )
    }
    fn event(sequence: u64, event: CodingRuntimeEvent) -> SequencedRuntimeEvent {
        SequencedRuntimeEvent {
            generation: 0,
            sequence,
            event,
        }
    }

    #[test]
    fn lead_model_survives_completed_turn_rebase_and_ignores_members_and_stale_events() {
        let mut view = view();
        let description = |session: &str, model: &str| {
            CodingRuntimeEvent::Agent(AgentEvent::Described {
                description: atomcode_kernel::agent::AgentDescription {
                    session: session.into(),
                    model: Some(model.into()),
                    ..Default::default()
                }
                .into(),
            })
        };
        view.observe(&event(1, description("old", "lumen-alpha")), None);
        view.observe(&event(2, description("old/member", "member-model")), None);
        view.observe(
            &event(
                3,
                CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    snapshot: Arc::new(SessionSnapshot::new(vec![Message::user("new base")])),
                    stats: Default::default(),
                }),
            ),
            None,
        );
        assert!(view.events.is_empty());
        assert_eq!(view.model.as_deref(), Some("lumen-alpha"));
        view.carry_generation(1);
        assert!(view.model.is_none());
        view.observe(
            &SequencedRuntimeEvent {
                generation: 1,
                sequence: 4,
                event: CodingRuntimeEvent::ProviderChanged {
                    provider: "same".into(),
                    model: "new-model".into(),
                },
            },
            None,
        );
        view.observe(&event(5, description("old", "stale-model")), None);
        assert_eq!(view.model.as_deref(), Some("new-model"));
    }

    #[test]
    fn a_replacement_without_a_seed_never_exposes_the_previous_session() {
        let mut view = view();
        let change = event(
            1,
            CodingRuntimeEvent::SessionChanged(SessionChanged {
                generation: RuntimeGeneration(1),
                session_id: Some("new".into()),
                working_dir: "/new".into(),
            }),
        );
        view.observe(&change, None);
        assert!(!view.valid);
        assert_eq!(view.session_id, "new");
        view.observe(
            &change,
            Some((
                SessionSnapshot::new(vec![Message::user("new history")]),
                RuntimeMode::Plan,
                None,
            )),
        );
        assert!(view.valid);
        assert_eq!(view.snapshot.messages[0].text, "new history");
        assert_eq!(view.mode, RuntimeMode::Plan);
        assert!(view.events.is_empty());
    }

    #[test]
    fn completion_rebases_without_losing_a_pending_goal_question() {
        let mut view = view();
        view.observe(
            &event(
                1,
                CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { turn: None }),
            ),
            None,
        );
        view.observe(
            &event(
                2,
                CodingRuntimeEvent::Request(RuntimeRequest {
                    id: 7,
                    kind: "request_user_input".into(),
                    payload: serde_json::json!({"goal_question_pending": true}),
                    snapshot: None,
                }),
            ),
            None,
        );
        view.observe(
            &event(
                3,
                CodingRuntimeEvent::Agent(AgentEvent::TextDelta("answer".into())),
            ),
            None,
        );
        view.observe(
            &event(
                4,
                CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    snapshot: Arc::new(SessionSnapshot::new(vec![Message::user("new base")])),
                    stats: Default::default(),
                }),
            ),
            None,
        );
        assert_eq!(view.events.len(), 1);
        assert!(
            matches!(&view.events[0].event, CodingRuntimeEvent::Request(request) if request.id == 7)
        );
        assert_eq!(view.sequence, Some(4));
        assert_eq!(view.snapshot.messages[0].text, "new base");
    }

    #[test]
    fn blocking_request_state_tracks_resolution_even_without_transcript_replay() {
        let mut view = view();
        view.valid = false;
        view.observe(
            &event(
                1,
                CodingRuntimeEvent::Request(RuntimeRequest {
                    id: 42,
                    kind: "approval".into(),
                    payload: serde_json::json!({}),
                    snapshot: None,
                }),
            ),
            None,
        );
        assert!(view.has_pending_blocking_request());
        view.observe(
            &event(
                2,
                CodingRuntimeEvent::RequestResolved {
                    id: 42,
                    kind: "approval".into(),
                },
            ),
            None,
        );
        assert!(!view.has_pending_blocking_request());
        assert!(!view.valid);
    }

    #[test]
    fn compaction_rebases_history_without_making_the_running_turn_idle() {
        let mut view = view();
        view.observe(
            &event(
                1,
                CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { turn: None }),
            ),
            None,
        );
        view.observe(
            &event(
                2,
                CodingRuntimeEvent::Agent(AgentEvent::TextDelta("old text".into())),
            ),
            None,
        );
        let mut outcome = CompactionOutcome::from_kernel(
            CompactTrigger::Auto { utilization: 0.9 },
            1,
            1,
            100,
            10,
            true,
            None,
        );
        outcome.committed_snapshot = Some(Arc::new(SessionSnapshot::new(vec![Message::user(
            "compacted",
        )])));
        view.observe(
            &event(
                3,
                CodingRuntimeEvent::CompactionFinished {
                    completion: CompactionCompletion::Completed(outcome),
                },
            ),
            None,
        );
        assert_eq!(view.snapshot.messages[0].text, "compacted");
        assert_eq!(view.events.len(), 1);
        assert!(matches!(
            view.events[0].event,
            CodingRuntimeEvent::Agent(AgentEvent::TurnStarted { .. })
        ));
        assert_eq!(view.sequence, Some(3));
    }

    #[test]
    fn adjacent_stream_chunks_preserve_text_and_the_sequence_boundary() {
        let mut view = view();
        for n in 0..20_000 {
            view.observe(
                &event(
                    n,
                    CodingRuntimeEvent::Agent(AgentEvent::TextDelta("x".into())),
                ),
                None,
            );
        }
        assert!(view.valid);
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].sequence, 19_999);
        assert_eq!(view.sequence, Some(19_999));
        assert!(
            matches!(&view.events[0].event, CodingRuntimeEvent::Agent(AgentEvent::TextDelta(text)) if text.len() == 20_000)
        );
    }

    #[test]
    fn tool_argument_streams_do_not_exhaust_the_replay_budget() {
        let mut view = view();
        for n in 0..20_000 {
            view.observe(
                &event(
                    n,
                    CodingRuntimeEvent::Agent(AgentEvent::ToolCallStreaming {
                        index: 0,
                        id: (n == 0).then(|| "call".into()),
                        name: (n == 0).then(|| "bash".into()),
                        arguments: "x".into(),
                    }),
                ),
                None,
            );
        }
        assert!(view.valid);
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].sequence, 19_999);
        assert!(matches!(&view.events[0].event,
            CodingRuntimeEvent::Agent(AgentEvent::ToolCallStreaming { id, name, arguments, .. })
            if id.as_deref() == Some("call") && name.as_deref() == Some("bash") && arguments.len() == 20_000));
        view.observe(
            &event(
                20_000,
                CodingRuntimeEvent::Agent(AgentEvent::ToolCallStreaming {
                    index: 1,
                    id: Some("other".into()),
                    name: Some("read".into()),
                    arguments: "y".into(),
                }),
            ),
            None,
        );
        assert_eq!(view.events.len(), 2, "different calls must stay separate");
    }

    #[test]
    fn replay_overflow_refuses_partial_initialization() {
        let mut view = view();
        for n in 0..=16_384 {
            view.observe(
                &event(n, CodingRuntimeEvent::ControllerWarning("x".into())),
                None,
            );
        }
        assert!(!view.valid);
        assert!(view.events.is_empty());
        view.observe(
            &event(
                16_385,
                CodingRuntimeEvent::Request(RuntimeRequest {
                    id: 8,
                    kind: "request_user_input".into(),
                    payload: serde_json::json!({"goal_question_pending":true}),
                    snapshot: None,
                }),
            ),
            None,
        );
        view.observe(
            &event(
                16_386,
                CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    snapshot: Arc::new(SessionSnapshot::new(Vec::new())),
                    stats: Default::default(),
                }),
            ),
            None,
        );
        assert!(view.valid);
        assert!(
            matches!(&view.events[0].event, CodingRuntimeEvent::Request(request) if request.id == 8)
        );
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn loop_progress_survives_rebase_and_ages_on_late_attachment() {
        let mut view = view();
        view.observe(
            &event(
                1,
                CodingRuntimeEvent::LoopChanged(LoopProgress {
                    active: true,
                    round: 1,
                    elapsed_secs: 0,
                    label: "check".into(),
                    last_reason: None,
                    recovery: Some(crate::GoalRecoveryProgress {
                        kind: GoalRecoveryKind::Provider,
                        waiting_for_quota: true,
                        attempt: 1,
                        max_attempts: 12,
                        retry_in_secs: 30,
                    }),
                }),
            ),
            None,
        );
        tokio::time::advance(std::time::Duration::from_secs(10)).await;
        view.observe(
            &event(
                2,
                CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                    turn_id: 1,
                    reason: StopReason::Stopped,
                    snapshot: Arc::new(SessionSnapshot::new(vec![])),
                    stats: RuntimeTurnStats::default(),
                }),
            ),
            None,
        );
        let read = view.sampled();
        assert!(read.events.iter().any(|event| matches!(&event.event, CodingRuntimeEvent::LoopChanged(p) if p.recovery.as_ref().is_some_and(|r| r.retry_in_secs == 20))));
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        for read in [read.sampled(), view.sampled()] {
            assert!(read.events.iter().any(|event| matches!(&event.event, CodingRuntimeEvent::LoopChanged(p) if p.recovery.as_ref().is_some_and(|r| r.retry_in_secs == 15))));
        }
    }
}
