//! Runtime-owned ordinary-question waits. Native tool receipts are the durable
//! facts; this map only routes live requests and never owns another agent.
use super::*;
use atomcode_capabilities::tools::request_user_input::PendingUserQuestion;
use atomcode_kernel::message::Role;
use serde_json::Value;
use tokio::time::Instant;

pub(super) const WAIT: std::time::Duration = std::time::Duration::from_secs(120);
pub(super) const PENDING_FLAG: &str = "goal_question_pending";

#[derive(Default)]
pub(super) struct GoalQuestions {
    pub generation: u64,
    pub waiting: BTreeMap<RequestId, (Instant, Value)>,
    pub continued: BTreeMap<RequestId, (PendingUserQuestion, bool)>,
    pub aliases: BTreeMap<RequestId, RequestId>,
    /// Only the first tool result can confirm or overtake a timer release.
    pub awaiting_receipt: BTreeSet<RequestId>,
    pub admitted_replies: BTreeSet<RequestId>,
    pub restored: bool,
}

impl GoalQuestions {
    pub fn restore_runtime(&mut self, runtime: &RuntimeResources) -> bool {
        let Some(root) = live_root_agent(runtime) else {
            return false;
        };
        self.restore_events(&root.session().events());
        true
    }

    pub fn restore_events(&mut self, events: &[atomcode_kernel::session::LoggedEvent]) {
        use atomcode_kernel::session::SessionEvent;
        let undone = atomcode_kernel::session::undone_turns(events);
        let rewound = atomcode_kernel::session::rewound_turns(events);
        let mut messages = Vec::new();
        let mut reply_contexts = BTreeMap::new();
        for (index, logged) in events.iter().enumerate() {
            if let SessionEvent::Injected { text, origin, .. } = &logged.event {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    if !rewound.contains(&logged.event.turn())
                        && matches!(origin, atomcode_kernel::session::InjectionOrigin::Reminder)
                        && value.get("goal_questions_closed").is_some()
                    {
                        messages.push(Message::system(text));
                        continue;
                    }
                    if matches!(
                        origin,
                        atomcode_kernel::session::InjectionOrigin::Continuation
                    ) && value.get("goal_question_reply").and_then(Value::as_str) == Some("v1")
                    {
                        // Context precedes the human message. It cannot settle
                        // anything until that same input actually enters the log.
                        reply_contexts.insert(logged.event.turn(), text.clone());
                    }
                }
            }
            if undone.contains(&logged.event.turn()) {
                continue;
            }
            match &logged.event {
                SessionEvent::AssistantMessage { tool_calls, .. } if !tool_calls.is_empty() => {
                    messages.push(Message::assistant(
                        "",
                        tool_calls
                            .iter()
                            .map(|call| atomcode_kernel::tool::ToolCall {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: String::new(),
                            })
                            .collect(),
                    ))
                }
                SessionEvent::ToolResultLogged {
                    call_id,
                    content,
                    is_error,
                    ..
                } => messages.push(Message::tool_result(
                    call_id,
                    if content.contains("\"pending\"") {
                        content.as_str()
                    } else {
                        ""
                    },
                    *is_error,
                )),
                SessionEvent::UserMessage { text, turn, .. } => {
                    if let Some(context) = reply_contexts.remove(turn) {
                        messages.push(Message::system(context));
                    }
                    if text.contains("\"goal_question_reply\"") {
                        messages.push(Message::user(text));
                    }
                }
                SessionEvent::Rewound { scope, .. } if scope.takes_back_conversation() => {
                    // Rewind changes which facts stand. Reuse the authoritative
                    // projection at that edge, rather than inventing undo logic.
                    messages = atomcode_kernel::session::derive_messages(&events[..=index]);
                }
                _ => {}
            }
        }
        // Compaction is not withdrawal: the full native log keeps unanswered
        // question identities even when model context no longer carries them.
        self.restore(&messages);
    }
    pub fn settle_receipt(&mut self, call_id: &str, content: &str) -> Vec<RequestId> {
        let recorded = serde_json::from_str::<Value>(content)
            .ok()
            .and_then(|value| value.get("pending").and_then(PendingUserQuestion::read))
            .map(|question| question.id);
        let matching: Vec<_> = self
            .awaiting_receipt
            .iter()
            .copied()
            .filter(|id| {
                self.continued.get(id).is_some_and(|(question, _)| {
                    question
                        .question
                        .get("question_call_id")
                        .and_then(Value::as_str)
                        == Some(call_id)
                })
            })
            .collect();
        let mut overtaken = Vec::new();
        for id in matching {
            self.awaiting_receipt.remove(&id);
            if recorded != Some(id) {
                self.continued.remove(&id);
                overtaken.push(id);
            }
        }
        overtaken
    }
    pub fn deadline(&self) -> Instant {
        self.waiting
            .values()
            .map(|(at, _)| *at)
            .min()
            .unwrap_or_else(|| Instant::now() + std::time::Duration::from_secs(86400))
    }
    pub fn waiting(&mut self, id: RequestId, payload: Value) {
        self.waiting.insert(id, (Instant::now() + WAIT, payload));
    }
    pub fn payload(question: &PendingUserQuestion) -> Value {
        let mut payload = question.question.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert(PENDING_FLAG.into(), Value::Bool(true));
        }
        payload
    }
    pub fn list(&self) -> Vec<RuntimeRequest> {
        self.continued
            .iter()
            .filter(|(_, (_, queued))| !queued)
            .map(|(id, (q, _))| RuntimeRequest {
                id: *id,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: Self::payload(q),
                snapshot: None,
            })
            .collect()
    }
    pub fn restore(&mut self, messages: &[Message]) {
        self.restored = true;
        let mut calls = BTreeMap::new();
        self.admitted_replies = messages
            .iter()
            .filter(|message| message.role == Role::System)
            .filter_map(|message| serde_json::from_str::<Value>(&message.text).ok())
            .filter(|value| value.get("goal_question_reply").and_then(Value::as_str) == Some("v1"))
            .filter_map(|value| value.get("id").and_then(Value::as_u64))
            .collect();
        let closed: BTreeSet<u64> = messages
            .iter()
            .filter(|message| message.role == Role::System)
            .filter_map(|message| serde_json::from_str::<Value>(&message.text).ok())
            .filter_map(|value| {
                if value.get("goal_question_reply").and_then(Value::as_str) == Some("v1") {
                    return value.get("id").cloned().map(|id| vec![id]);
                }
                value
                    .get("goal_questions_closed")
                    .and_then(Value::as_array)
                    .cloned()
            })
            .flatten()
            .filter_map(|value| value.as_u64())
            .collect();
        for message in messages {
            for call in &message.tool_calls {
                calls
                    .entry(call.id.as_str())
                    .and_modify(|entry| *entry = None)
                    .or_insert(Some(call.name.as_str()));
            }
            if message.role == Role::Tool {
                let Some(Some("ask_user" | "request_user_input")) = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| calls.remove(id))
                else {
                    continue;
                };
                if message.is_error {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<Value>(&message.text) else {
                    continue;
                };
                if let Some(pending) = value.get("pending").and_then(PendingUserQuestion::read) {
                    if self.continued.len() < 8 && !closed.contains(&pending.id) {
                        self.continued.entry(pending.id).or_insert((pending, false));
                    }
                }
            } else if message.role == Role::User && !message.synthetic {
                if let Ok(reply) = serde_json::from_str::<Value>(message.display_text()) {
                    if reply.get("goal_question_reply").and_then(Value::as_str) == Some("v1") {
                        if let Some(id) = reply.get("id").and_then(Value::as_u64) {
                            self.continued.remove(&id);
                        }
                    }
                }
            }
        }
        self.continued.retain(|id, _| !closed.contains(id));
    }

    pub fn close(
        &mut self,
        runtime: Option<&RuntimeResources>,
        requests: &mut BTreeMap<RequestId, String>,
        events: &RuntimeEventEmitter,
    ) {
        let ids: Vec<_> = self.continued.keys().copied().collect();
        if ids.is_empty() {
            return;
        }
        if let Some(root) = runtime.and_then(live_root_agent) {
            let log = root.session();
            // This records an accepted control fact now. Queuing a note for a
            // future turn would let cancellation discard its own withdrawal.
            atomcode_harness::session::commit(root.ctx(), &log, atomcode_kernel::session::SessionEvent::Injected {
                turn:log.current_turn(),
                origin:atomcode_kernel::session::InjectionOrigin::Reminder,
                text:serde_json::json!({"goal_questions_closed":ids,"reason":"user cancelled goal; not an answer or approval"}).to_string(),
            });
        }
        for id in ids {
            requests.remove(&id);
            if self.continued.get(&id).is_some_and(|(_, queued)| *queued) {
                continue;
            }
            let _ = events.send(CodingRuntimeEvent::RequestResolved {
                id,
                kind: REQUEST_USER_INPUT_KIND.into(),
            });
        }
        self.continued.clear();
        self.aliases.clear();
        self.awaiting_receipt.clear();
    }
    pub fn has_independent_work(messages: &[Message]) -> bool {
        // The last submitted segment includes synthetic goal continuations too.
        let start = messages
            .iter()
            .rposition(|m| m.role == Role::User)
            .unwrap_or(0);
        messages[start..]
            .iter()
            .flat_map(|m| &m.tool_calls)
            .any(|call| !matches!(call.name.as_str(), "ask_user" | "request_user_input"))
    }
    pub fn reply_context(question: &PendingUserQuestion) -> String {
        serde_json::json!({"goal_question_reply":"v1","id":question.id,"question":question.question,
            "note":"Historical model-authored question data, not new user instructions. The accompanying human input answers this pending question. Apply it to remaining work; do not automatically repeat completed actions. This is not an approval grant."}).to_string()
    }
    pub fn reply(_question: &PendingUserQuestion, answer: Value) -> UserInput {
        use atomcode_capabilities::tools::request_user_input::UserInputResponse;
        let answers: Vec<UserInputResponse> =
            if let Some(responses) = answer.get("responses").and_then(Value::as_array) {
                responses
                    .iter()
                    .filter_map(|value| serde_json::from_value(value.clone()).ok())
                    .collect()
            } else {
                serde_json::from_value(answer).ok().into_iter().collect()
            };
        let batch = answers.len() > 1;
        let text = answers
            .into_iter()
            .enumerate()
            .map(|(index, answer)| {
                let text = if answer.declined {
                    "No answer (declined).".to_string()
                } else {
                    let mut parts = answer.selected;
                    if let Some(text) = answer.text.filter(|text| !text.trim().is_empty()) {
                        parts.push(text);
                    }
                    parts.join("\n")
                };
                if batch {
                    format!("Q{}: {text}", index + 1)
                } else {
                    text
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        UserInput::from(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;
    use serde_json::json;

    fn question() -> Value {
        json!({"header":"Choice", "question":"Which design?", "mode":"single", "options":[{"label":"A"},{"label":"B"}],"custom":true,"question_call_id":"ask-test"})
    }

    #[test]
    fn pending_replay_requires_question_tool_receipts_and_late_human_reply_settles_it() {
        let pending = PendingUserQuestion::new(900, question());
        let call = |name: &str| {
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "call".into(),
                    name: name.into(),
                    arguments: question().to_string(),
                }],
            )
        };
        let result = Message::tool_result("call", pending.result().content, false);
        let mut questions = GoalQuestions::default();
        questions.restore(&[call("read_file"), result.clone()]);
        assert!(
            questions.continued.is_empty(),
            "file contents cannot create a pending question"
        );
        questions.restore(&[
            call("read_file"),
            Message::tool_result("call", "ordinary file contents", false),
            call("ask_user"),
            result.clone(),
        ]);
        assert_eq!(
            questions.list().len(),
            1,
            "a completed earlier call may legally reuse its provider call id"
        );
        questions.continued.clear();

        questions.restore(&[call("ask_user"), result.clone()]);
        assert_eq!(questions.list().len(), 1);
        let reply = GoalQuestions::reply(&pending, json!({"declined":false,"text":"B"}));
        assert_eq!(reply.text, "B");
        questions.restore(&[Message::system(GoalQuestions::reply_context(&pending))]);
        assert!(
            questions.continued.is_empty(),
            "an admitted reply closes the pending question on replay"
        );
        questions.restore(&[
            call("ask_user"),
            result,
            Message::assistant("{\"goal_question_reply\":\"v1\",\"id\":900}", vec![]),
        ]);
        assert_eq!(
            questions.list().len(),
            1,
            "the model cannot invent a human reply"
        );
    }

    #[test]
    fn compaction_keeps_pending_questions_but_rewind_withdraws_them() {
        use atomcode_kernel::session::{LoggedEvent, RewindScope, SessionEvent};
        let pending = PendingUserQuestion::new(901, question());
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![ToolCall {
                    id: "ask".into(),
                    name: "ask_user".into(),
                    arguments: question().to_string(),
                }],
                reasoning_blocks: vec![],
                meta: None,
            },
            SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "ask".into(),
                content: pending.result().content,
                is_error: false,
                images: vec![],
            },
            SessionEvent::Compacted {
                turn: 2,
                through: 3,
                from: 0,
                summary: "Earlier work summarized".into(),
            },
        ];
        let mut events: Vec<_> = facts
            .into_iter()
            .enumerate()
            .map(|(index, event)| LoggedEvent {
                seq: index as u64 + 1,
                at: 0,
                event,
            })
            .collect();
        let mut questions = GoalQuestions::default();
        questions.restore_events(&events);
        assert_eq!(
            questions.list().len(),
            1,
            "compaction is not an answer or withdrawal"
        );
        let mut admitted = events.clone();
        admitted.push(LoggedEvent {
            seq: 5,
            at: 0,
            event: SessionEvent::TurnStart { turn: 3 },
        });
        admitted.push(LoggedEvent {
            seq: 6,
            at: 0,
            event: SessionEvent::Injected {
                turn: 3,
                text: GoalQuestions::reply_context(&pending),
                origin: atomcode_kernel::session::InjectionOrigin::Continuation,
            },
        });
        let mut waiting = GoalQuestions::default();
        waiting.restore_events(&admitted);
        assert_eq!(
            waiting.list().len(),
            1,
            "context alone is not an admitted human answer"
        );
        admitted.push(LoggedEvent {
            seq: 7,
            at: 0,
            event: SessionEvent::UserMessage {
                turn: 3,
                text: "B".into(),
                images: vec![],
            },
        });
        let mut answered = GoalQuestions::default();
        answered.restore_events(&admitted);
        assert!(
            answered.list().is_empty(),
            "context and its same-turn human message did not settle the question"
        );
        assert!(answered.admitted_replies.contains(&901));

        let mut cancelled = events.clone();
        cancelled.push(LoggedEvent {
            seq: 5,
            at: 0,
            event: SessionEvent::TurnStart { turn: 2 },
        });
        cancelled.push(LoggedEvent {
            seq: 6,
            at: 0,
            event: SessionEvent::Injected {
                turn: 2,
                text: json!({"goal_questions_closed":[901]}).to_string(),
                origin: atomcode_kernel::session::InjectionOrigin::Reminder,
            },
        });
        cancelled.push(LoggedEvent {
            seq: 7,
            at: 0,
            event: SessionEvent::Interrupted {
                turn: 2,
                undone: true,
            },
        });
        let mut closed = GoalQuestions::default();
        closed.restore_events(&cancelled);
        assert!(
            closed.list().is_empty(),
            "discarding interrupted work discarded its own cancellation"
        );

        events.push(LoggedEvent {
            seq: 5,
            at: 0,
            event: SessionEvent::Rewound {
                turn: 3,
                to: 1,
                scope: RewindScope::Conversation,
            },
        });
        let mut rewound = GoalQuestions::default();
        rewound.restore_events(&events);
        assert!(
            rewound.list().is_empty(),
            "rewound question was resurrected"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn ordinary_question_deadline_is_bounded_and_approval_is_not_registered() {
        let mut questions = GoalQuestions::default();
        questions.waiting(7, question());
        assert_eq!(questions.deadline() - Instant::now(), WAIT);
        tokio::time::advance(WAIT).await;
        assert!(questions.deadline() <= Instant::now());
    }

    #[test]
    fn confirming_a_receipt_does_not_let_a_reused_call_id_close_the_question() {
        let pending = PendingUserQuestion::new(902, question());
        let mut questions = GoalQuestions::default();
        questions.continued.insert(902, (pending.clone(), false));
        questions.awaiting_receipt.insert(902);
        assert!(questions
            .settle_receipt("ask-test", &pending.result().content)
            .is_empty());
        assert!(questions
            .settle_receipt("ask-test", "The person answered: B")
            .is_empty());
        assert_eq!(questions.list().len(), 1);
        questions.awaiting_receipt.insert(902);
        assert_eq!(
            questions.settle_receipt("ask-test", "The person answered: B"),
            vec![902]
        );
        assert!(questions.list().is_empty());
    }

    #[test]
    fn late_batch_answers_keep_their_question_boundaries() {
        let pending = PendingUserQuestion::new(903, question());
        let answer = GoalQuestions::reply(
            &pending,
            json!({"responses":[
                {"declined":false,"selected":["A","B"]},
                {"declined":false,"text":"C"},
                {"declined":true}
            ]}),
        );
        assert_eq!(answer.text, "Q1: A\nB\nQ2: C\nQ3: No answer (declined).");
    }

    #[test]
    fn asking_again_is_not_independent_work() {
        let messages = |tool: &str| {
            vec![
                Message::user("continue"),
                Message::assistant(
                    "",
                    vec![ToolCall {
                        id: "call".into(),
                        name: tool.into(),
                        arguments: "{}".into(),
                    }],
                ),
            ]
        };
        assert!(!GoalQuestions::has_independent_work(&messages("ask_user")));
        assert!(GoalQuestions::has_independent_work(&messages("read_file")));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::super::tests::controller_test_runtime;
    use super::*;
    use serde_json::json;

    struct Factory;
    impl CodingProviderFactory for Factory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                vec![],
            )))
        }
    }
    fn payload() -> Value {
        json!({"header":"Design","question":"A or B?","mode":"single","options":[{"label":"A"},{"label":"B"}],"custom":true,"question_call_id":"ask-test"})
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_goal_question_times_out_as_pending_and_a_late_reply_is_submitted_once() {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(Factory)).await;
        handle.start_goal("ship it").await.unwrap();
        commands.recv().await.unwrap();
        events
            .send(AgentEvent::Request {
                id: 7,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: payload(),
            })
            .unwrap();
        while !matches!(output.recv().await, Some(CodingRuntimeEvent::Request(_))) {}
        tokio::time::advance(WAIT).await;
        let response = commands.recv().await.unwrap();
        let AgentCommand::Respond { id, value } = response else {
            panic!("question not released");
        };
        assert_eq!(id, 7);
        let pending = PendingUserQuestion::read(&value).expect("not an answer or decline");
        let retained = handle.goal_questions().await.unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].id, pending.id);
        assert_eq!(retained[0].payload[PENDING_FLAG], true);
        handle
            .respond(pending.id, json!({"declined":false,"text":"B"}))
            .await
            .unwrap();
        let delivered = commands.recv().await.unwrap();
        let AgentCommand::SendMessageWithContext { text, context, .. } = delivered else {
            panic!("late reply lost its admission context");
        };
        assert_eq!(text, "B");
        let reply: Value = serde_json::from_str(&context).unwrap();
        assert_eq!(reply["id"], pending.id);
        assert!(handle.goal_questions().await.unwrap().is_empty());
        assert!(matches!(
            handle
                .respond(pending.id, json!({"declined":false,"text":"B"}))
                .await,
            Err(RuntimeError::StaleRequest { .. } | RuntimeError::Busy)
        ));
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_tool_that_already_closed_cannot_be_resurrected_by_the_goal_timer() {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(Factory)).await;
        handle.start_goal("ship it").await.unwrap();
        commands.recv().await.unwrap();
        events
            .send(AgentEvent::Request {
                id: 7,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: payload(),
            })
            .unwrap();
        while !matches!(output.recv().await, Some(CodingRuntimeEvent::Request(_))) {}
        tokio::time::advance(WAIT).await;
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::Respond { .. })
        ));
        // The driver had already ended its own shorter wait. Its result won,
        // while the runtime timer's response was already in transit.
        events
            .send(AgentEvent::ToolResult {
                result: atomcode_kernel::tool::ToolResult {
                    call_id: "ask-test".into(),
                    content: "No answer was provided.".into(),
                    is_error: false,
                    images: vec![],
                },
            })
            .unwrap();
        loop {
            if matches!(
                output.recv().await,
                Some(CodingRuntimeEvent::Agent(AgentEvent::ToolResult { .. }))
            ) {
                break;
            }
        }
        assert!(handle.goal_questions().await.unwrap().is_empty());
        tokio::time::advance(WAIT * 2).await;
        assert!(commands.try_recv().is_err());
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn on_time_answers_and_approvals_keep_their_original_round_trip() {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(Factory)).await;
        handle.start_goal("ship it").await.unwrap();
        commands.recv().await.unwrap();
        for (id, kind) in [
            (7, REQUEST_USER_INPUT_KIND),
            (8, APPROVAL_KIND),
            (9, REQUEST_USER_INPUT_KIND),
        ] {
            events
                .send(AgentEvent::Request {
                    id,
                    kind: kind.into(),
                    payload: if id == 9 {
                        let mut value = payload();
                        value["authorization_required"] = Value::Bool(true);
                        value
                    } else {
                        payload()
                    },
                })
                .unwrap();
            loop {
                if matches!(output.recv().await,Some(CodingRuntimeEvent::Request(ref r)) if r.id==id)
                {
                    break;
                }
            }
            if kind == APPROVAL_KIND || id == 9 {
                tokio::time::advance(WAIT * 2).await;
                assert!(
                    commands.try_recv().is_err(),
                    "approval was answered by a timer"
                );
            }
            let value = json!({"declined":false,"text":"B"});
            handle.respond(id, value).await.unwrap();
            assert!(
                matches!(commands.recv().await,Some(AgentCommand::Respond{id:got,..}) if got==id)
            );
        }
        assert!(handle.goal_questions().await.unwrap().is_empty());
        handle.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod pause_tests {
    use super::super::tests::controller_test_runtime;
    use super::*;
    use atomcode_kernel::tool::ToolCall;
    use serde_json::json;
    struct Factory;
    impl CodingProviderFactory for Factory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                vec![],
            )))
        }
    }
    fn payload() -> Value {
        json!({"header":"Choice","question":"Which account?","mode":"text","custom":true,"question_call_id":"ask-test"})
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_round_with_only_a_pending_question_pauses_instead_of_replaying_work() {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(Factory)).await;
        handle
            .start_goal("deploy to the chosen account")
            .await
            .unwrap();
        commands.recv().await.unwrap();
        events
            .send(AgentEvent::Request {
                id: 7,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: payload(),
            })
            .unwrap();
        while !matches!(output.recv().await, Some(CodingRuntimeEvent::Request(_))) {}
        tokio::time::advance(WAIT).await;
        let AgentCommand::Respond { value, .. } = commands.recv().await.unwrap() else {
            panic!("missing pending result");
        };
        let pending = PendingUserQuestion::read(&value).unwrap();
        events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped,
                provider_failure: None,
            })
            .unwrap();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![
                    Message::user("deploy to the chosen account"),
                    Message::assistant(
                        "",
                        vec![ToolCall {
                            id: "ask".into(),
                            name: "ask_user".into(),
                            arguments: payload().to_string(),
                        }],
                    ),
                    Message::tool_result("ask", pending.result().content, false),
                    Message::assistant(
                        "Cannot deploy without an account. No other independent work remains.",
                        vec![],
                    ),
                ]),
            })
            .unwrap();
        let mut paused = false;
        loop {
            match output.recv().await {
                Some(CodingRuntimeEvent::GoalChanged(p)) if !p.active => {
                    paused = true;
                    assert!(p.last_reason.unwrap_or_default().contains("待答问题"));
                }
                Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                Some(_) => {}
                None => panic!("missing turn terminal"),
            }
        }
        assert!(paused);
        assert!(
            commands.try_recv().is_err(),
            "idle goal dispatched another main/evaluator operation"
        );
        assert_eq!(handle.goal_questions().await.unwrap().len(), 1);
        handle.shutdown().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn an_ordinary_non_goal_question_has_no_goal_deadline() {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(Factory)).await;
        handle.submit(UserInput::from("work")).await.unwrap();
        commands.recv().await.unwrap();
        events
            .send(AgentEvent::Request {
                id: 7,
                kind: REQUEST_USER_INPUT_KIND.into(),
                payload: payload(),
            })
            .unwrap();
        while !matches!(output.recv().await, Some(CodingRuntimeEvent::Request(_))) {}
        tokio::time::advance(WAIT * 2).await;
        assert!(commands.try_recv().is_err());
        handle
            .respond(7, json!({"declined":false,"text":"account"}))
            .await
            .unwrap();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::Respond { id: 7, .. })
        ));
        handle.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ToolCall, ToolDef};
    use futures::stream::BoxStream;
    use serde_json::json;
    struct Provider(Arc<AtomicU64>);
    #[async_trait::async_trait]
    impl LlmProvider for Provider {
        fn model_name(&self) -> &str {
            "pending-question-test"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            tools: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            let evaluation = messages
                .first()
                .is_some_and(|m| m.text.starts_with("You are a strict goal evaluator"));
            let event = if evaluation {
                StreamEvent::TextDelta("Verdict: yes work verified".into())
            } else if tools.iter().any(|tool| tool.name == "request_user_input")
                && self.0.fetch_add(1, Ordering::SeqCst) == 0
            {
                StreamEvent::ToolCall(ToolCall {
                    id: "ask-native".into(),
                    name: "request_user_input".into(),
                    arguments: json!({"question":"Which account?","header":"Account","mode":"single","options":[{"label":"A"},{"label":"B"}]}).to_string(),
                })
            } else {
                StreamEvent::TextDelta(
                    "No independent work remains until the account is chosen.".into(),
                )
            };
            Ok(Box::pin(futures::stream::iter([
                event,
                StreamEvent::Done { truncated: false },
            ])))
        }
    }
    struct Factory(Arc<AtomicU64>);
    impl CodingProviderFactory for Factory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(Provider(self.0.clone())))
        }
    }
    fn input(
        root: &std::path::Path,
        calls: &Arc<AtomicU64>,
        mode: crate::SessionMode,
    ) -> CodingRuntimeStart {
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut input = super::super::tests::native_start(false);
        input.agent = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "pending-question-test",
            project,
            atomcode_capabilities::ProductDirs::new(root.join("user"), ".project"),
        );
        input.prepare.session = mode;
        input.provider_factory = Arc::new(Factory(calls.clone()));
        input
    }
    async fn finished(runtime: &mut CodingRuntime) {
        tokio::time::timeout(std::time::Duration::from_secs(600), async {
            loop {
                let event = runtime
                    .events
                    .recv()
                    .await
                    .expect("runtime closed before terminal");
                if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
                    break;
                }
            }
        })
        .await
        .expect("turn did not finish");
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn native_pending_question_survives_restart_and_its_late_answer_does_not_reopen() {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let mut runtime =
            CodingRuntime::start(input(root.path(), &calls, crate::SessionMode::Fresh))
                .await
                .unwrap();
        let session = runtime.session.as_ref().unwrap().id.clone();
        runtime
            .handle
            .start_goal("work on the chosen account")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(600), async {
            loop {
                let event = runtime
                    .events
                    .recv()
                    .await
                    .expect("runtime closed before question");
                assert!(
                    !matches!(event.event, CodingRuntimeEvent::TurnFinished(_)),
                    "turn ended before it asked the expected question"
                );
                if matches!(event.event, CodingRuntimeEvent::Request(_)) {
                    break;
                }
            }
        })
        .await
        .expect("question was not requested");
        tokio::time::advance(WAIT).await;
        finished(&mut runtime).await;
        let pending = runtime.handle.goal_questions().await.unwrap();
        assert_eq!(pending.len(), 1);
        let id = pending[0].id;
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let mut runtime = CodingRuntime::start(input(
            root.path(),
            &calls,
            crate::SessionMode::Resume(session.clone()),
        ))
        .await
        .unwrap();
        assert_eq!(runtime.handle.goal_questions().await.unwrap()[0].id, id);
        runtime
            .handle
            .respond(id, json!({"declined":false,"text":"B"}))
            .await
            .unwrap();
        finished(&mut runtime).await;
        assert!(runtime.handle.goal_questions().await.unwrap().is_empty());
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let runtime = CodingRuntime::start(input(
            root.path(),
            &calls,
            crate::SessionMode::Resume(session),
        ))
        .await
        .unwrap();
        assert!(
            runtime.handle.goal_questions().await.unwrap().is_empty(),
            "settled pending question reopened after restart"
        );
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelled_native_questions_are_not_offered_again_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicU64::new(0));
        let mut runtime =
            CodingRuntime::start(input(root.path(), &calls, crate::SessionMode::Fresh))
                .await
                .unwrap();
        let session = runtime.session.as_ref().unwrap().id.clone();
        runtime
            .handle
            .start_goal("work on the chosen account")
            .await
            .unwrap();
        finished(&mut runtime).await;
        assert_eq!(runtime.handle.goal_questions().await.unwrap().len(), 1);
        runtime.handle.cancel().await.unwrap();
        assert!(runtime.handle.goal_questions().await.unwrap().is_empty());
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let runtime = CodingRuntime::start(input(
            root.path(),
            &calls,
            crate::SessionMode::Resume(session),
        ))
        .await
        .unwrap();
        assert!(
            runtime.handle.goal_questions().await.unwrap().is_empty(),
            "cancelled question reopened after restart"
        );
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }
}

#[cfg(test)]
mod closure_tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;
    use serde_json::json;
    #[test]
    fn cancellation_is_not_an_answer_and_a_closed_question_does_not_reopen() {
        let q = PendingUserQuestion::new(
            900,
            json!({"header":"Account","question":"Which?","mode":"text"}),
        );
        let messages = vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "ask".into(),
                    name: "ask_user".into(),
                    arguments: "{}".into(),
                }],
            ),
            Message::tool_result("ask", q.result().content, false),
            Message::system(
                json!({"goal_questions_closed":[900],"reason":"user cancelled"}).to_string(),
            ),
        ];
        let mut questions = GoalQuestions::default();
        questions.restore(&messages);
        assert!(questions.list().is_empty());
    }
}
