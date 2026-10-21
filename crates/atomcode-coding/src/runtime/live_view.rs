//! Read-only presentation projection. Never used to resume or mutate an agent.
use super::*;

/// A complete baseline plus every runtime event since it, through `sequence`.
#[derive(Clone, Debug)]
pub struct RuntimeLiveView {
    pub generation: u64,
    pub mode: RuntimeMode,
    pub session_id: String,
    pub working_dir: std::path::PathBuf,
    pub snapshot: Arc<SessionSnapshot>,
    pub events: Vec<SequencedRuntimeEvent>,
    pub sequence: Option<u64>,
    pub(crate) valid: bool,
    pending: BTreeMap<RequestId, SequencedRuntimeEvent>,
    goal: Option<SequencedRuntimeEvent>,
    policy: Option<SequencedRuntimeEvent>,
    turn: Option<SequencedRuntimeEvent>,
}

impl RuntimeLiveView {
    pub fn from_snapshot(
        generation: u64,
        session_id: String,
        working_dir: std::path::PathBuf,
        snapshot: SessionSnapshot,
    ) -> Self {
        Self {
            generation,
            mode: RuntimeMode::Build,
            session_id,
            working_dir,
            snapshot: Arc::new(snapshot),
            events: Vec::new(),
            sequence: None,
            valid: true,
            pending: BTreeMap::new(),
            goal: None,
            policy: None,
            turn: None,
        }
    }

    /// A harness provider patch keeps the same running conversation and requests.
    pub(super) fn carry_generation(&mut self, generation: u64) {
        self.generation = generation;
        for event in &mut self.events {
            event.generation = generation;
        }
        for event in self.pending.values_mut() {
            event.generation = generation;
        }
        for event in self
            .goal
            .iter_mut()
            .chain(self.policy.iter_mut())
            .chain(self.turn.iter_mut())
        {
            event.generation = generation;
        }
    }

    pub(super) fn observe(
        &mut self,
        envelope: &SequencedRuntimeEvent,
        seed: Option<(SessionSnapshot, RuntimeMode)>,
    ) {
        if let CodingRuntimeEvent::SessionChanged(changed) = &envelope.event {
            self.generation = envelope.generation;
            self.session_id = changed.session_id.clone().unwrap_or_default();
            self.working_dir = changed.working_dir.clone();
            self.events.clear();
            self.pending.clear();
            self.goal = None;
            self.policy = None;
            self.turn = None;
            self.valid = seed.is_some();
            if let Some((snapshot, mode)) = seed {
                self.snapshot = Arc::new(snapshot);
                self.mode = mode;
            }
        } else if envelope.generation != self.generation {
            self.valid = false;
        } else {
            if let CodingRuntimeEvent::ModeChanged { mode } = &envelope.event {
                self.mode = *mode;
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
                CodingRuntimeEvent::GoalChanged(_) => {
                    self.goal = Some(envelope.clone());
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
                self.events.extend(self.goal.clone());
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
}
