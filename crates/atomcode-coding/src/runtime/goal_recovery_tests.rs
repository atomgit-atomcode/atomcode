use super::tests::controller_test_runtime;
use super::*;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use std::time::Duration;

#[derive(Default)]
struct EvaluatorFactory {
    calls: AtomicU64,
    failures: u64,
    invalid: bool,
}
impl CodingProviderFactory for EvaluatorFactory {
    fn build(
        &self,
        _: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if call < self.failures {
            if self.invalid {
                vec![
                    StreamEvent::TextDelta("Verdict: yesterday".into()),
                    StreamEvent::Done { truncated: false },
                ]
            } else {
                vec![StreamEvent::Error(ProviderError {
                    retryable: true,
                    http_status: Some(503),
                    message: "temporarily unavailable".into(),
                    ..Default::default()
                })]
            }
        } else {
            vec![
                StreamEvent::TextDelta("Verdict: yes tests passed".into()),
                StreamEvent::Done { truncated: false },
            ]
        };
        Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
            events,
        ])))
    }
}
fn terminal_error(status: u16) -> AgentEvent {
    AgentEvent::Error {
        message: "provider refused".into(),
        http_status: Some(status),
        code: None,
        retryable: Some(true),
        retry_after_secs: None,
        ends_turn: true,
    }
}
async fn close_kernel_turn(
    events: &mpsc::UnboundedSender<AgentEvent>,
    commands: &mut mpsc::UnboundedReceiver<AgentCommand>,
    reason: StopReason,
) {
    events
        .send(AgentEvent::TurnComplete {
            turn: None,
            reason,
            provider_failure: None,
        })
        .unwrap();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::Snapshot)
    ));
    events
        .send(AgentEvent::Snapshot {
            snapshot: SessionSnapshot::new(vec![Message::assistant(
                "already completed file edits",
                vec![],
            )]),
        })
        .unwrap();
}
async fn scheduled(
    events: &mut mpsc::UnboundedReceiver<CodingRuntimeEvent>,
    kind: GoalRecoveryKind,
) -> crate::GoalRecoveryProgress {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(CodingRuntimeEvent::GoalChanged(progress)) = events.recv().await {
                if let Some(recovery) = progress.recovery {
                    if recovery.attempt == 0 || recovery.retry_in_secs == 0 {
                        continue;
                    }
                    assert_eq!(recovery.kind, kind);
                    return recovery;
                }
            }
        }
    })
    .await
    .expect("goal never entered recovery")
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_transient_main_failure_waits_then_finishes_the_same_foreground_operation() {
    let factory = Arc::new(EvaluatorFactory::default());
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(factory.clone()).await;
    handle.start_goal("tests pass").await.unwrap();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendMessage { .. })
    ));
    events.send(terminal_error(503)).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
    let wait = scheduled(&mut output, GoalRecoveryKind::Provider).await;
    assert!(
        commands.try_recv().is_err(),
        "must cool down rather than immediately reissue work"
    );
    let began = tokio::time::Instant::now();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert!(began.elapsed() >= Duration::from_secs(wait.retry_in_secs.saturating_sub(1)));
    assert!(began.elapsed() <= Duration::from_secs(31));
    events.send(AgentEvent::TurnStarted { turn: None }).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::Stopped).await;
    let mut terminals = 0;
    loop {
        match output.recv().await {
            Some(CodingRuntimeEvent::TurnFinished(_)) => {
                terminals += 1;
                break;
            }
            Some(_) => {}
            None => panic!("missing foreground terminal"),
        }
    }
    while let Ok(event) = output.try_recv() {
        if matches!(event, CodingRuntimeEvent::TurnFinished(_)) {
            terminals += 1;
        }
    }
    assert_eq!(terminals, 1);
    assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn evaluator_transient_and_malformed_replies_retry_only_the_verdict() {
    for invalid in [false, true] {
        let factory = Arc::new(EvaluatorFactory {
            failures: 2,
            invalid,
            ..Default::default()
        });
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(factory.clone()).await;
        handle.start_goal("tests pass").await.unwrap();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::SendMessage { .. })
        ));
        close_kernel_turn(&events, &mut commands, StopReason::Stopped).await;
        scheduled(&mut output, GoalRecoveryKind::Evaluator).await;
        let met = tokio::time::timeout(Duration::from_secs(3600), async {
            let mut met = false;
            loop {
                match output.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(progress)) => {
                        met |= progress.terminal == Some(GoalTerminal::Met);
                    }
                    Some(CodingRuntimeEvent::TurnFinished(_)) => break met,
                    Some(_) => {}
                    None => panic!("missing evaluator terminal"),
                }
            }
        })
        .await
        .unwrap();
        assert!(met);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
        assert!(
            commands.try_recv().is_err(),
            "evaluation retry must never replay main work"
        );
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn authentication_and_quota_failures_do_not_start_goal_recovery() {
    for status in [401, 403, 429] {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
        handle.start_goal("tests pass").await.unwrap();
        commands.recv().await.unwrap();
        events.send(terminal_error(status)).unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
        loop {
            match output.recv().await {
                Some(CodingRuntimeEvent::GoalChanged(progress)) => {
                    assert!(progress.recovery.is_none())
                }
                Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                Some(_) => {}
                None => panic!("missing terminal"),
            }
        }
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(commands.try_recv().is_err());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_pause_and_user_input_invalidate_the_pending_cooldown() {
    for action in ["cancel", "pause", "submit", "shutdown"] {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
        handle.start_goal("tests pass").await.unwrap();
        commands.recv().await.unwrap();
        events.send(terminal_error(503)).unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
        scheduled(&mut output, GoalRecoveryKind::Provider).await;
        match action {
            "cancel" => handle.cancel().await.unwrap(),
            "pause" => handle.pause_goal().await.unwrap(),
            "shutdown" => {
                handle.shutdown().await.unwrap();
            }
            _ => {
                handle
                    .submit(UserInput::from("new instruction"))
                    .await
                    .unwrap();
                assert!(
                    matches!(commands.recv().await, Some(AgentCommand::SendMessage { text, .. }) if text == "new instruction")
                );
            }
        }
        tokio::time::advance(Duration::from_secs(3600)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(command) = commands.try_recv() {
            assert!(
                action == "shutdown"
                    && matches!(command, AgentCommand::Cancel | AgentCommand::Shutdown),
                "stale recovery dispatched after {action}: {command:?}"
            );
        }
        if action != "shutdown" {
            handle.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repeated_timed_rate_limits_wait_then_resume_without_replaying_tools() {
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
    handle.start_goal("tests pass").await.unwrap();
    commands.recv().await.unwrap();
    for attempt in 1..=3 {
        events
            .send(AgentEvent::Error {
                message: "limited".into(),
                http_status: Some(429),
                code: Some("rate_limit_exceeded".into()),
                retryable: Some(true),
                retry_after_secs: Some(120),
                ends_turn: true,
            })
            .unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
        let progress = scheduled(&mut output, GoalRecoveryKind::Provider).await;
        assert_eq!(progress.attempt, attempt);
        assert!(progress.retry_in_secs >= 120);
        let began = tokio::time::Instant::now();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::SendSyntheticMessage { .. })
        ));
        assert!(began.elapsed() >= Duration::from_secs(120));
        events.send(AgentEvent::TurnStarted { turn: None }).unwrap();
    }
    close_kernel_turn(&events, &mut commands, StopReason::Stopped).await;
    let mut met = false;
    while let Some(event) = output.recv().await {
        match event {
            CodingRuntimeEvent::GoalChanged(progress) => {
                met |= progress.terminal == Some(GoalTerminal::Met)
            }
            CodingRuntimeEvent::TurnFinished(_) => break,
            _ => {}
        }
    }
    assert!(met);
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_long_wait_is_visible_and_cancellable_before_any_new_request() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = CodingAgentConfig::new(
        "key",
        "https://example.test/v1",
        "test",
        tmp.path(),
        atomcode_capabilities::ProductDirs::new(tmp.path().join("user"), ".project"),
    );
    let mut registry = atomcode_config::Config::default();
    registry.goal.recovery.max_elapsed_secs = 1;
    registry.goal.recovery.long_wait = Some(Default::default());
    config.subagent_config = Some(Arc::new(registry));
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        super::tests::controller_test_runtime_with_config(
            Arc::new(EvaluatorFactory::default()),
            config,
        )
        .await;
    handle.start_goal("tests pass").await.unwrap();
    commands.recv().await.unwrap();
    events.send(terminal_error(503)).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
    let progress = scheduled(&mut output, GoalRecoveryKind::Provider).await;
    assert_eq!(progress.retry_in_secs, 900);
    assert!(commands.try_recv().is_err());
    handle.cancel().await.unwrap();
    tokio::time::advance(Duration::from_secs(28_800)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_clean_rate_limit_pause_can_recover_only_with_transient_terminal_metadata() {
    for retryable in [true, false] {
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
        handle.start_goal("tests pass").await.unwrap();
        commands.recv().await.unwrap();
        events
            .send(AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::RateLimited,
                provider_failure: Some(atomcode_kernel::event::ProviderFailure {
                    retryable,
                    http_status: Some(429),
                    code: Some("rate_limit_exceeded".into()),
                    retry_after_secs: Some(901),
                }),
            })
            .unwrap();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::Snapshot)
        ));
        events
            .send(AgentEvent::Snapshot {
                snapshot: SessionSnapshot::new(vec![]),
            })
            .unwrap();
        if retryable {
            let progress = scheduled(&mut output, GoalRecoveryKind::Provider).await;
            assert_eq!(progress.retry_in_secs, 901);
        } else {
            while let Some(event) = output.recv().await {
                match event {
                    CodingRuntimeEvent::GoalChanged(progress) => {
                        assert!(progress.recovery.is_none())
                    }
                    CodingRuntimeEvent::TurnFinished(_) => break,
                    _ => {}
                }
            }
        }
        handle.cancel().await.unwrap();
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn policy_blocking_a_recovery_closes_the_foreground_and_clears_busy() {
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
    handle.start_goal("tests pass").await.unwrap();
    commands.recv().await.unwrap();
    events.send(terminal_error(503)).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::ProviderError).await;
    scheduled(&mut output, GoalRecoveryKind::Provider).await;
    events
        .send(AgentEvent::PolicyIntervention {
            intervention: atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(),
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match output.recv().await {
                Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                Some(_) => {}
                None => panic!("missing foreground terminal"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(handle.status().phase, RuntimePhase::Ready);
    assert!(handle.pending_policy_intervention().await.is_some());
    assert!(
        commands.try_recv().is_err(),
        "policy must block automatic continuation"
    );
    handle.shutdown().await.unwrap();
}

async fn observe_work(
    events: &mpsc::UnboundedSender<AgentEvent>,
    output: &mut mpsc::UnboundedReceiver<CodingRuntimeEvent>,
    prompt: u32,
    completion: u32,
) {
    events
        .send(AgentEvent::ToolStarted {
            call: atomcode_kernel::tool::ToolCall {
                id: format!("work-{prompt}"),
                name: "read_file".into(),
                arguments: "{}".into(),
            },
        })
        .unwrap();
    events
        .send(AgentEvent::Usage(MessageMeta {
            tokens: atomcode_kernel::stream::TokenUsage {
                prompt,
                completion,
                ..Default::default()
            },
            ..Default::default()
        }))
        .unwrap();
    while !matches!(
        output.recv().await,
        Some(CodingRuntimeEvent::Agent(AgentEvent::Usage(_)))
    ) {}
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn live_pause_and_final_drain_retain_observed_work_without_double_counting() {
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
    handle.start_goal("finish").await.unwrap();
    commands.recv().await.unwrap();
    observe_work(&events, &mut output, 100, 10).await;
    let report = handle.goal_report().await.unwrap();
    assert!(report.contains("工具调用：1 · tokens：110"), "{report}");
    handle.pause_goal().await.unwrap();
    assert!(matches!(commands.recv().await, Some(AgentCommand::Cancel)));
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::Snapshot)
    ));
    // A provider can publish its final usage while cancellation drains.
    observe_work(&events, &mut output, 200, 20).await;
    events
        .send(AgentEvent::TurnComplete {
            turn: None,
            reason: StopReason::Cancelled,
            provider_failure: None,
        })
        .unwrap();
    events
        .send(AgentEvent::Snapshot {
            snapshot: SessionSnapshot::new(vec![]),
        })
        .unwrap();
    while !matches!(
        output.recv().await,
        Some(CodingRuntimeEvent::TurnFinished(_))
    ) {}
    let report = handle.goal_report().await.unwrap();
    assert!(report.contains("工具调用：2 · tokens：330"), "{report}");
    // Unrelated agent work while the retained goal is idle must not be charged.
    observe_work(&events, &mut output, 999, 1).await;
    assert_eq!(handle.goal_report().await.unwrap(), report);
    handle.submit(UserInput::from("continue")).await.unwrap();
    commands.recv().await.unwrap();
    observe_work(&events, &mut output, 300, 30).await;
    close_kernel_turn(&events, &mut commands, StopReason::Stopped).await;
    while !matches!(
        output.recv().await,
        Some(CodingRuntimeEvent::TurnFinished(_))
    ) {}
    let report = handle.goal_report().await.unwrap();
    assert!(report.contains("工具调用：3 · tokens：660"), "{report}");
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancelling_keeps_already_observed_statistics_in_the_terminal_report() {
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(EvaluatorFactory::default())).await;
    handle.start_goal("finish").await.unwrap();
    commands.recv().await.unwrap();
    observe_work(&events, &mut output, 100, 10).await;
    handle.cancel().await.unwrap();
    let report = handle.goal_report().await.unwrap();
    assert!(report.contains("工具调用：1 · tokens：110"), "{report}");
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn explicitly_unknown_evidence_pauses_without_dispatching_more_main_work() {
    struct UnknownFactory;
    impl CodingProviderFactory for UnknownFactory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                    vec![
                        StreamEvent::TextDelta(
                            "Verdict: unknown Build receipt absent".into()
                        ),
                        StreamEvent::Done { truncated: false },
                    ];
                    10
                ])))
        }
    }
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(UnknownFactory)).await;
    handle.start_goal("build passes").await.unwrap();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendMessage { .. })
    ));
    close_kernel_turn(&events, &mut commands, StopReason::Stopped).await;
    let mut stopped = false;
    tokio::time::timeout(Duration::from_secs(3600), async {
        loop {
            match output.recv().await {
                Some(CodingRuntimeEvent::GoalChanged(progress)) if !progress.active => {
                    assert_ne!(progress.terminal, Some(GoalTerminal::Met));
                    assert_ne!(progress.terminal, Some(GoalTerminal::Failed));
                    stopped |= progress.terminal == Some(GoalTerminal::Stopped);
                }
                Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                Some(_) => {}
                None => panic!("missing goal terminal"),
            }
        }
    })
    .await
    .unwrap();
    assert!(stopped);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), commands.recv())
            .await
            .is_err(),
        "missing evidence must not submit synthetic main work"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_unknown_after_evidence_expansion_stops_without_recovery_or_main_replay() {
    struct UnknownFactory(Arc<AtomicU64>);
    impl CodingProviderFactory for UnknownFactory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(vec![
                vec![
                    StreamEvent::TextDelta(
                        "Verdict: unknown insufficient observed validation".into()
                    ),
                    StreamEvent::Done { truncated: false },
                ];
                2
            ])))
        }
    }
    let builds = Arc::new(AtomicU64::new(0));
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        controller_test_runtime(Arc::new(UnknownFactory(builds.clone()))).await;
    handle.start_goal("tests pass").await.unwrap();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendMessage { .. })
    ));
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
                Message::assistant(
                    "",
                    vec![atomcode_kernel::tool::ToolCall {
                        id: "tests".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"command":"python3 -m unittest"}).to_string(),
                    }],
                ),
                Message::tool_result("tests", "Ran 28 tests; OK", false),
            ]),
        })
        .unwrap();
    let mut paused = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match output.recv().await {
                Some(CodingRuntimeEvent::GoalChanged(progress)) if !progress.active => {
                    assert_eq!(progress.phase, GoalPhase::PausedAtCap);
                    assert_eq!(progress.report.evaluator_attempts, 2);
                    assert!(progress
                        .last_reason
                        .as_deref()
                        .unwrap_or("")
                        .contains("bounded logged evidence"));
                    paused = true;
                }
                Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                Some(_) => {}
                None => panic!("missing terminal"),
            }
        }
    })
    .await
    .unwrap();
    assert!(paused);
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[derive(Debug)]
struct QuotaSource {
    calls: AtomicU64,
    account: AtomicU64,
    answers: std::sync::Mutex<
        std::collections::VecDeque<Result<Vec<crate::rate_limit::RateLimitWindow>, String>>,
    >,
}
#[async_trait::async_trait]
impl crate::rate_limit::RateLimitWindowSource for QuotaSource {
    fn applies_to(&self, _: &str) -> bool {
        true
    }
    fn account_id(&self) -> Option<String> {
        let account = self.account.load(Ordering::SeqCst);
        (account != 0).then(|| account.to_string())
    }
    async fn fetch_windows(&self) -> Result<Vec<crate::rate_limit::RateLimitWindow>, String> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // Goal startup also asks this source to size its round cap.
            return Ok(vec![]);
        }
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no answer".into()))
    }
}
fn quota_window(exhausted: bool, seconds: i64) -> crate::rate_limit::RateLimitWindow {
    crate::rate_limit::RateLimitWindow {
        window_size_seconds: 18000,
        quota_exhausted: exhausted,
        reset_at_display: "18:00".into(),
        seconds_until_reset: seconds,
        reset_label: "5h".into(),
        usage_percent: 0.0,
        calls_used: 0,
        call_limit: 0,
    }
}
fn quota_pause(seconds: Option<u64>) -> AgentEvent {
    AgentEvent::RateLimited {
        reset_at_display: "18:00".into(),
        reset_label: "5h".into(),
        secs_until_reset: seconds,
        auto_resuming: false,
        server_message: None,
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_quota_paused_goal_checks_the_allowance_at_reset_before_continuing() {
    let mut retired = quota_window(true, 999999);
    retired.window_size_seconds = 30 * 86400;
    let source = Arc::new(QuotaSource {
        calls: AtomicU64::new(0),
        account: AtomicU64::new(1),
        answers: std::sync::Mutex::new([Ok(vec![quota_window(false, 18000), retired])].into()),
    });
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(EvaluatorFactory::default()),
            super::tests::native_start(false).agent,
            Some(source.clone()),
        )
        .await;
    handle.start_goal("finish the edits").await.unwrap();
    commands.recv().await.unwrap();
    events.send(quota_pause(Some(301))).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::RateLimited).await;
    let wait = scheduled(&mut output, GoalRecoveryKind::Provider).await;
    assert_eq!(wait.retry_in_secs, 301);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    let began = tokio::time::Instant::now();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert!(began.elapsed() >= Duration::from_secs(301));
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    handle.cancel().await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_quota_probe_cannot_resume_on_exhausted_unknown_or_failed_windows() {
    for first in [
        Ok(vec![quota_window(true, 401)]),
        Ok(vec![]),
        Err("offline".into()),
    ] {
        let source = Arc::new(QuotaSource {
            calls: AtomicU64::new(0),
            account: AtomicU64::new(1),
            answers: std::sync::Mutex::new([first, Ok(vec![quota_window(false, 18000)])].into()),
        });
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(EvaluatorFactory::default()),
                super::tests::native_start(false).agent,
                Some(source.clone()),
            )
            .await;
        handle.start_goal("finish the edits").await.unwrap();
        commands.recv().await.unwrap();
        events.send(quota_pause(Some(301))).unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::RateLimited).await;
        scheduled(&mut output, GoalRecoveryKind::Provider).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(301)).await;
        let wait = scheduled(&mut output, GoalRecoveryKind::Provider).await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
        assert!(
            commands.try_recv().is_err(),
            "must not dispatch before a successful check"
        );
        assert!(wait.attempt >= 2);
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::SendSyntheticMessage { .. })
        ));
        assert_eq!(source.calls.load(Ordering::SeqCst), 3);
        handle.cancel().await.unwrap();
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn user_actions_invalidate_a_pending_quota_wait() {
    for action in [
        "cancel",
        "pause",
        "submit",
        "shutdown",
        "account_switch",
        "sign_out",
    ] {
        let source = Arc::new(QuotaSource {
            calls: AtomicU64::new(0),
            account: AtomicU64::new(1),
            answers: std::sync::Mutex::new([Ok(vec![quota_window(false, 0)])].into()),
        });
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(EvaluatorFactory::default()),
                super::tests::native_start(false).agent,
                Some(source.clone()),
            )
            .await;
        handle.start_goal("finish").await.unwrap();
        commands.recv().await.unwrap();
        events.send(quota_pause(Some(301))).unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::RateLimited).await;
        scheduled(&mut output, GoalRecoveryKind::Provider).await;
        match action {
            "sign_out" => {
                source.account.store(0, Ordering::SeqCst);
            }
            "account_switch" => {
                source.account.store(2, Ordering::SeqCst);
            }
            "cancel" => handle.cancel().await.unwrap(),
            "pause" => handle.pause_goal().await.unwrap(),
            "shutdown" => {
                handle.shutdown().await.unwrap();
            }
            _ => {
                handle
                    .submit(UserInput::from("new instruction"))
                    .await
                    .unwrap();
                assert!(
                    matches!(commands.recv().await, Some(AgentCommand::SendMessage { text, .. }) if text == "new instruction")
                );
            }
        }
        tokio::time::advance(Duration::from_secs(1000)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        while let Ok(command) = commands.try_recv() {
            assert!(!matches!(
                command,
                AgentCommand::SendSyntheticMessage { .. }
            ));
        }
        if matches!(action, "account_switch" | "sign_out") {
            let mut paused = false;
            while let Ok(event) = output.try_recv() {
                if let CodingRuntimeEvent::GoalChanged(progress) = event {
                    paused |= !progress.active
                        && progress
                            .last_reason
                            .as_deref()
                            .is_some_and(|note| note.contains("account changed"));
                }
            }
            assert!(paused, "an account switch must explicitly pause the goal");
        }
        if action != "shutdown" {
            handle.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn quota_waits_do_not_bypass_existing_budgets_or_unknown_reset_times() {
    for case in [
        "recovery_disabled",
        "recovery_time",
        "goal_time",
        "unknown_reset",
    ] {
        let source = Arc::new(QuotaSource {
            calls: AtomicU64::new(0),
            account: AtomicU64::new(1),
            answers: std::sync::Mutex::new([Ok(vec![quota_window(false, 0)])].into()),
        });
        let mut config = super::tests::native_start(false).agent;
        let mut registry = atomcode_config::Config::default();
        if case == "recovery_disabled" {
            registry.goal.recovery.max_attempts = 0;
        }
        if case == "goal_time" {
            config.goal_max_duration_secs = 100;
        }
        config.subagent_config = Some(Arc::new(registry));
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(EvaluatorFactory::default()),
                config,
                Some(source.clone()),
            )
            .await;
        handle.start_goal("finish").await.unwrap();
        commands.recv().await.unwrap();
        events
            .send(quota_pause(if case == "unknown_reset" {
                None
            } else if case == "recovery_time" {
                Some(1801)
            } else {
                Some(301)
            }))
            .unwrap();
        close_kernel_turn(&events, &mut commands, StopReason::RateLimited).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match output.recv().await {
                    Some(CodingRuntimeEvent::GoalChanged(progress)) => {
                        assert!(progress.recovery.is_none())
                    }
                    Some(CodingRuntimeEvent::TurnFinished(_)) => break,
                    Some(_) => {}
                    None => panic!("missing terminal"),
                }
            }
        })
        .await
        .unwrap();
        tokio::time::advance(Duration::from_secs(2000)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(commands.try_recv().is_err());
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_opted_in_long_quota_wait_checks_before_resuming() {
    let source = Arc::new(QuotaSource {
        calls: AtomicU64::new(0),
        account: AtomicU64::new(1),
        answers: std::sync::Mutex::new([Ok(vec![quota_window(false, 18000)])].into()),
    });
    let mut config = super::tests::native_start(false).agent;
    let mut registry = atomcode_config::Config::default();
    registry.goal.recovery.long_wait = Some(Default::default());
    config.subagent_config = Some(Arc::new(registry));
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(EvaluatorFactory::default()),
            config,
            Some(source.clone()),
        )
        .await;
    handle.start_goal("finish").await.unwrap();
    commands.recv().await.unwrap();
    events.send(quota_pause(Some(7200))).unwrap();
    close_kernel_turn(&events, &mut commands, StopReason::RateLimited).await;
    let wait = scheduled(&mut output, GoalRecoveryKind::Provider).await;
    assert_eq!(wait.retry_in_secs, 7200);
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    let began = tokio::time::Instant::now();
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert!(began.elapsed() >= Duration::from_secs(7200));
    assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    handle.cancel().await.unwrap();
    handle.shutdown().await.unwrap();
}
