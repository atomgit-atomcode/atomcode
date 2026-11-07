use super::*;
use std::time::Duration;

struct Factory;
impl CodingProviderFactory for Factory {
    fn build(
        &self,
        _: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn atomcode_kernel::provider::LlmProvider>, crate::ProviderBuildError> {
        Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
            vec![],
        )))
    }
}
#[derive(Debug)]
struct Quota {
    account: AtomicU64,
    calls: AtomicU64,
    delay: u64,
    answers: Mutex<VecDeque<Result<Vec<crate::rate_limit::RateLimitWindow>, String>>>,
}
impl Quota {
    fn new(
        answers: Vec<Result<Vec<crate::rate_limit::RateLimitWindow>, String>>,
        delay: u64,
    ) -> Arc<Self> {
        Arc::new(Self {
            account: AtomicU64::new(1),
            calls: AtomicU64::new(0),
            delay,
            answers: Mutex::new(answers.into()),
        })
    }
}
#[async_trait::async_trait]
impl crate::rate_limit::RateLimitWindowSource for Quota {
    fn applies_to(&self, _: &str) -> bool {
        true
    }
    fn account_id(&self) -> Option<String> {
        let id = self.account.load(Ordering::SeqCst);
        (id != 0).then(|| id.to_string())
    }
    async fn fetch_windows(&self) -> Result<Vec<crate::rate_limit::RateLimitWindow>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(self.delay)).await;
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("offline".into()))
    }
}
fn window(exhausted: bool, seconds: i64) -> crate::rate_limit::RateLimitWindow {
    crate::rate_limit::RateLimitWindow {
        window_size_seconds: 18000,
        quota_exhausted: exhausted,
        reset_at_display: "18:00".into(),
        seconds_until_reset: seconds,
        reset_label: "5h".into(),
        call_limit: 0,
        calls_used: 0,
        usage_percent: 0.0,
    }
}
fn pause() -> AgentEvent {
    AgentEvent::RateLimited {
        reset_at_display: "18:00".into(),
        reset_label: "5h".into(),
        secs_until_reset: Some(301),
        auto_resuming: false,
        server_message: None,
    }
}
async fn close(
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
            snapshot: SessionSnapshot::new(vec![]),
        })
        .unwrap();
}
async fn waiting(output: &mut mpsc::UnboundedReceiver<CodingRuntimeEvent>) -> LoopProgress {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(CodingRuntimeEvent::LoopChanged(progress)) = output.recv().await {
                if progress
                    .recovery
                    .as_ref()
                    .is_some_and(|r| r.retry_in_secs > 0)
                {
                    return progress;
                }
            }
        }
    })
    .await
    .unwrap()
}
async fn ended(output: &mut mpsc::UnboundedReceiver<CodingRuntimeEvent>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            output.recv().await,
            Some(CodingRuntimeEvent::TurnFinished(_))
        ) {}
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn quota_recovery_finishes_the_same_iteration_then_keeps_the_original_cadence() {
    for every in [Some(60), None] {
        let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
        let (handle, mut commands, events, mut output, wakeups, active, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(Factory),
                super::tests::native_start(false).agent,
                Some(quota.clone()),
            )
            .await;
        handle.start_loop("check CI", every).await.unwrap();
        commands.recv().await.unwrap();
        if every.is_none() {
            wakeups
                .send(WakeupRequest {
                    delay_seconds: 42,
                    prompt: "model next".into(),
                    reason: "next requested".into(),
                })
                .unwrap();
            loop {
                if matches!(output.recv().await, Some(CodingRuntimeEvent::LoopChanged(p)) if p.last_reason.as_deref().is_some_and(|reason| reason.starts_with("scheduled in")))
                {
                    break;
                }
            }
        }
        events.send(pause()).unwrap();
        close(&events, &mut commands, StopReason::RateLimited).await;
        let before = waiting(&mut output).await;
        assert!(active.load(Ordering::SeqCst));
        assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
        let began = tokio::time::Instant::now();
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::SendSyntheticMessage { .. })
        ));
        assert!(began.elapsed() >= Duration::from_secs(301));
        assert_eq!(
            handle.autonomy().await.unwrap().looping.unwrap().round,
            before.round
        );
        events.send(AgentEvent::TurnStarted { turn: None }).unwrap();
        close(&events, &mut commands, StopReason::Stopped).await;
        let began = tokio::time::Instant::now();
        let expected = if every.is_some() {
            "check CI"
        } else {
            "model next"
        };
        assert!(
            matches!(commands.recv().await, Some(AgentCommand::SendMessage { text, .. }) if text == expected)
        );
        assert!(began.elapsed() >= Duration::from_secs(every.unwrap_or(42) as u64));
        assert_eq!(
            handle.autonomy().await.unwrap().looping.unwrap().round,
            before.round + 1
        );
        handle.cancel().await.unwrap();
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_loop_refresh_checks_early_without_duplicate_work_or_a_new_iteration() {
    let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 1);
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(Factory),
            super::tests::native_start(false).agent,
            Some(quota.clone()),
        )
        .await;
    handle.start_loop("check", Some(60)).await.unwrap();
    commands.recv().await.unwrap();
    events.send(pause()).unwrap();
    close(&events, &mut commands, StopReason::RateLimited).await;
    let before = waiting(&mut output).await;
    let began = tokio::time::Instant::now();
    assert!(handle.refresh_quota_wait().await.unwrap());
    assert!(handle.refresh_quota_wait().await.unwrap());
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(
        handle.autonomy().await.unwrap().looping.unwrap().round,
        before.round
    );
    tokio::time::advance(Duration::from_secs(301)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(quota.calls.load(Ordering::SeqCst), 1);
    assert!(commands.try_recv().is_err());
    handle.cancel().await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_loop_never_continues_on_exhausted_unknown_or_failed_quota() {
    for first in [
        Ok(vec![window(true, 401)]),
        Ok(vec![]),
        Err("offline".into()),
    ] {
        let quota = Quota::new(vec![first, Ok(vec![window(false, 18000)])], 0);
        let (handle, mut commands, events, mut output, _, _, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(Factory),
                super::tests::native_start(false).agent,
                Some(quota.clone()),
            )
            .await;
        handle.start_loop("check", Some(60)).await.unwrap();
        commands.recv().await.unwrap();
        events.send(pause()).unwrap();
        close(&events, &mut commands, StopReason::RateLimited).await;
        let before = waiting(&mut output).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(302)).await;
        let after = waiting(&mut output).await;
        assert_eq!(after.round, before.round);
        assert_eq!(after.recovery.unwrap().attempt, 2);
        assert_eq!(quota.calls.load(Ordering::SeqCst), 1);
        assert!(commands.try_recv().is_err());
        assert!(handle.refresh_quota_wait().await.unwrap());
        assert!(matches!(
            commands.recv().await,
            Some(AgentCommand::SendSyntheticMessage { .. })
        ));
        assert_eq!(quota.calls.load(Ordering::SeqCst), 2);
        handle.cancel().await.unwrap();
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_new_input_replacement_and_shutdown_invalidate_a_loop_quota_wait() {
    for action in ["cancel", "stop", "input", "loop", "shutdown"] {
        let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
        let (handle, mut commands, events, mut output, _, active, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(Factory),
                super::tests::native_start(false).agent,
                Some(quota.clone()),
            )
            .await;
        handle.start_loop("old task", Some(60)).await.unwrap();
        commands.recv().await.unwrap();
        events.send(pause()).unwrap();
        close(&events, &mut commands, StopReason::RateLimited).await;
        waiting(&mut output).await;
        match action {
            "cancel" => handle.cancel().await.unwrap(),
            "stop" => handle.stop_loop().await.unwrap(),
            "shutdown" => {
                handle.shutdown().await.unwrap();
            }
            "loop" => {
                handle.start_loop("replacement", Some(10)).await.unwrap();
                assert!(
                    matches!(commands.recv().await, Some(AgentCommand::SendMessage { text, .. }) if text == "replacement")
                );
            }
            _ => {
                assert!(matches!(
                    handle.submit(UserInput::from("new task")).await.unwrap(),
                    SubmitReceipt::Started { .. }
                ));
                assert!(
                    matches!(commands.recv().await, Some(AgentCommand::SendMessage { text, .. }) if text == "new task")
                );
                assert!(handle.autonomy().await.unwrap().looping.is_none());
            }
        }
        tokio::time::advance(Duration::from_secs(400)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
        while let Ok(command) = commands.try_recv() {
            assert!(!matches!(
                command,
                AgentCommand::SendSyntheticMessage { .. }
            ));
        }
        if action != "loop" {
            assert!(!active.load(Ordering::SeqCst), "{action}");
        }
        if action != "shutdown" {
            handle.shutdown().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_account_switch_cannot_rebind_a_waiting_loop() {
    let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
    let (handle, mut commands, events, mut output, _, active, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(Factory),
            super::tests::native_start(false).agent,
            Some(quota.clone()),
        )
        .await;
    handle.start_loop("check", Some(60)).await.unwrap();
    commands.recv().await.unwrap();
    events.send(pause()).unwrap();
    close(&events, &mut commands, StopReason::RateLimited).await;
    waiting(&mut output).await;
    quota.account.store(2, Ordering::SeqCst);
    assert!(handle.refresh_quota_wait().await.unwrap());
    ended(&mut output).await;
    assert!(!active.load(Ordering::SeqCst));
    assert!(handle.autonomy().await.unwrap().looping.is_none());
    assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn quota_recovery_cannot_override_a_loop_safety_stop_or_recovery_budget() {
    for case in ["max_rounds", "disabled", "expired"] {
        let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
        let mut config = super::tests::native_start(false).agent;
        let mut registry = atomcode_config::Config::default();
        if case == "disabled" {
            registry.goal.recovery.max_attempts = 0;
        }
        if case == "expired" {
            registry.goal.recovery.max_elapsed_secs = 10;
        }
        config.subagent_config = Some(Arc::new(registry));
        let (handle, mut commands, events, mut output, _, active, _adapter) =
            super::tests::controller_test_runtime_with_quota(
                Arc::new(Factory),
                config,
                Some(quota.clone()),
            )
            .await;
        handle.start_loop("check", Some(60)).await.unwrap();
        commands.recv().await.unwrap();
        events.send(pause()).unwrap();
        close(
            &events,
            &mut commands,
            if case == "max_rounds" {
                StopReason::MaxRounds
            } else {
                StopReason::RateLimited
            },
        )
        .await;
        ended(&mut output).await;
        assert!(!handle.refresh_quota_wait().await.unwrap());
        assert!(!active.load(Ordering::SeqCst));
        tokio::time::advance(Duration::from_secs(400)).await;
        assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
        assert!(commands.try_recv().is_err());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn quota_recovery_can_finish_the_last_allowed_iteration_without_granting_another() {
    let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
    let mut config = super::tests::native_start(false).agent;
    config.loop_max_rounds = 1;
    let (handle, mut commands, events, mut output, _, active, _adapter) =
        super::tests::controller_test_runtime_with_quota(Arc::new(Factory), config, Some(quota))
            .await;
    handle.start_loop("check", Some(1)).await.unwrap();
    commands.recv().await.unwrap();
    close(&events, &mut commands, StopReason::Stopped).await;
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendMessage { .. })
    ));
    assert_eq!(handle.autonomy().await.unwrap().looping.unwrap().round, 1);
    events.send(pause()).unwrap();
    close(&events, &mut commands, StopReason::RateLimited).await;
    waiting(&mut output).await;
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert_eq!(handle.autonomy().await.unwrap().looping.unwrap().round, 1);
    close(&events, &mut commands, StopReason::Stopped).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(CodingRuntimeEvent::TurnFinished(TurnCompletion::Completed {
                reason,
                ..
            })) = output.recv().await
            {
                assert_eq!(reason, StopReason::MaxRounds);
                break;
            }
        }
    })
    .await
    .unwrap();
    assert!(!active.load(Ordering::SeqCst));
    assert!(commands.try_recv().is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_old_cadence_timer_cannot_start_a_new_iteration_under_a_quota_wait() {
    let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
    let (handle, mut commands, events, mut output, _, _, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(Factory),
            super::tests::native_start(false).agent,
            Some(quota.clone()),
        )
        .await;
    handle.start_loop("check", Some(10)).await.unwrap();
    commands.recv().await.unwrap();
    close(&events, &mut commands, StopReason::Stopped).await;
    // Another authorized kernel turn opens before the cadence expires.
    events.send(AgentEvent::TurnStarted { turn: None }).unwrap();
    events.send(pause()).unwrap();
    close(&events, &mut commands, StopReason::RateLimited).await;
    waiting(&mut output).await;
    tokio::time::advance(Duration::from_secs(11)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(commands.try_recv().is_err());
    assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
    assert!(handle.refresh_quota_wait().await.unwrap());
    assert!(matches!(
        commands.recv().await,
        Some(AgentCommand::SendSyntheticMessage { .. })
    ));
    assert_eq!(handle.autonomy().await.unwrap().looping.unwrap().round, 0);
    handle.cancel().await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_autonomous_wakeup_invalidates_the_old_quota_timer_but_keeps_the_loop() {
    let quota = Quota::new(vec![Ok(vec![window(false, 18000)])], 0);
    let (handle, mut commands, events, mut output, _, active, _adapter) =
        super::tests::controller_test_runtime_with_quota(
            Arc::new(Factory),
            super::tests::native_start(false).agent,
            Some(quota.clone()),
        )
        .await;
    handle.start_loop("check", Some(60)).await.unwrap();
    commands.recv().await.unwrap();
    events.send(pause()).unwrap();
    close(&events, &mut commands, StopReason::RateLimited).await;
    waiting(&mut output).await;
    events.send(AgentEvent::TurnStarted { turn: None }).unwrap();
    loop {
        if matches!(output.recv().await, Some(CodingRuntimeEvent::LoopChanged(p)) if p.recovery.is_none() && p.active)
        {
            break;
        }
    }
    tokio::time::advance(Duration::from_secs(400)).await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(quota.calls.load(Ordering::SeqCst), 0);
    assert!(commands.try_recv().is_err());
    assert!(active.load(Ordering::SeqCst));
    assert!(handle.autonomy().await.unwrap().looping.unwrap().active);
    handle.shutdown().await.unwrap();
}
