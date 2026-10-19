//! Controller metadata only. No conversation, grants, pending calls or provider state.
use super::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct LogMarker {
    bytes: u64,
    modified_ns: u64,
}
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    saved_at_ms: i64,
    session_id: String,
    working_dir: PathBuf,
    marker: LogMarker,
    progress: GoalProgress,
}
struct Bound {
    manager: Arc<atomcode_capabilities::session::SessionManager>,
    lease: atomcode_capabilities::session::SessionLease,
    path: PathBuf,
    working_dir: PathBuf,
}
impl Bound {
    fn marker(&self) -> Result<LogMarker, String> {
        let path = self
            .manager
            .events_path(self.lease.id())
            .map_err(|e| e.to_string())?;
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        let modified = meta
            .modified()
            .map_err(|e| e.to_string())?
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?;
        Ok(LogMarker {
            bytes: meta.len(),
            modified_ns: modified.as_nanos().min(u64::MAX as u128) as u64,
        })
    }
}
#[derive(Default)]
pub(super) struct GoalCheckpoints {
    bound: Option<Bound>,
    saved: Option<GoalProgress>,
    blocked: Option<String>,
}
impl GoalCheckpoints {
    pub fn available(&self) -> Result<(), RuntimeError> {
        self.blocked
            .as_ref()
            .map_or(Ok(()), |e| Err(RuntimeError::GoalUnavailable(e.clone())))
    }
    pub fn commit(&mut self, emitter: &RuntimeEventEmitter) -> Result<(), RuntimeError> {
        self.available()?;
        self.flush(emitter).map_err(RuntimeError::GoalUnavailable)
    }
    fn flush_inner(&mut self, progress: Option<GoalProgress>) -> Result<(), String> {
        if self.blocked.is_some() || progress == self.saved {
            return Ok(());
        }
        if let (Some(bound), Some(progress)) = (&self.bound, progress.as_ref()) {
            let value = Checkpoint {
                version: 1,
                saved_at_ms: atomcode_capabilities::session::now_ms(),
                session_id: bound.lease.id().into(),
                working_dir: bound.working_dir.clone(),
                marker: bound.marker()?,
                progress: progress.clone(),
            };
            bound
                .manager
                .write_controller_checkpoint(&bound.lease, &value)
                .map_err(|e| e.to_string())?;
        }
        self.saved = progress;
        Ok(())
    }
    pub fn flush(&mut self, emitter: &RuntimeEventEmitter) -> Result<(), String> {
        let progress = emitter
            .latest_goal
            .lock()
            .expect("goal projection poisoned")
            .clone();
        match self.flush_inner(progress) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.blocked = Some(e.clone());
                Err(e)
            }
        }
    }
    pub fn sync(
        &mut self,
        runtime: Option<&RuntimeResources>,
        goal: &mut Option<GoalState>,
        next_id: &mut u64,
        emitter: &RuntimeEventEmitter,
    ) -> Result<(), String> {
        let binding = runtime.and_then(|r| r.parts.session.as_ref());
        let path = binding
            .map(|s| s.manager.meta_path(&s.id))
            .transpose()
            .map_err(|e| e.to_string())?;
        if path.as_ref() == self.bound.as_ref().map(|b| &b.path) {
            return self.flush(emitter);
        }
        if let Err(error) = self.flush(emitter) {
            let _ = emitter.send(CodingRuntimeEvent::ControllerWarning(format!(
                "离开会话时目标检查点未保存：{error}"
            )));
        }
        self.saved = None;
        self.blocked = None;
        self.bound = binding.map(|s| Bound {
            manager: s.manager.clone(),
            lease: s.lease.clone(),
            path: path.clone().expect("bound path"),
            working_dir: runtime.expect("bound runtime").config.working_dir.clone(),
        });
        *emitter
            .latest_goal
            .lock()
            .expect("goal projection poisoned") = None;
        let Some(bound) = self.bound.as_ref() else {
            return Ok(());
        };
        let loaded = (|| -> Result<Option<Checkpoint>, String> {
            let value: Option<Checkpoint> = bound
                .manager
                .read_controller_checkpoint(&bound.lease)
                .map_err(|e| e.to_string())?;
            if let Some(value) = &value {
                if value.version != 1
                    || value.session_id != bound.lease.id()
                    || value.working_dir != bound.working_dir
                    || value.progress.condition.len() > 32_768
                {
                    return Err("目标检查点版本或会话标识不匹配；未恢复，也不会覆盖它。".into());
                }
            }
            Ok(value)
        })();
        let mut loaded = match loaded {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(()),
            Err(e) => {
                self.blocked = Some(e.clone());
                return Err(e);
            }
        };
        if loaded.marker != bound.marker()? {
            loaded.progress.evaluation_ready = false;
        }
        self.saved = Some(loaded.progress.clone());
        if loaded.progress.terminal.is_none()
            || matches!(
                loaded.progress.phase,
                GoalPhase::Paused | GoalPhase::PausedAtCap
            )
        {
            let age = atomcode_capabilities::session::now_ms()
                .saturating_sub(loaded.saved_at_ms)
                .max(0) as u64
                / 1000;
            loaded.progress.elapsed_secs = loaded.progress.elapsed_secs.saturating_add(age);
            if let Some(remaining) = loaded.progress.report.remaining_time_secs.as_mut() {
                *remaining = remaining.saturating_sub(age);
            }
        }
        if matches!(
            loaded.progress.phase,
            GoalPhase::Pursuing | GoalPhase::Paused | GoalPhase::PausedAtCap
        ) {
            *next_id = next_id.wrapping_add(1);
            let mut restored = GoalState::restore_paused(*next_id, &loaded.progress);
            if let Some(registry) = runtime.and_then(|r| r.config.subagent_config.as_ref()) {
                restored.recovery_policy = registry.goal.recovery.clone();
            }
            let _ = emitter.send(CodingRuntimeEvent::GoalChanged(restored.progress()));
            *goal = Some(restored);
        } else {
            *emitter
                .latest_goal
                .lock()
                .expect("goal projection poisoned") = Some(loaded.progress);
        }
        Ok(())
    }
}

pub(super) fn report(progress: &GoalProgress) -> String {
    let r = &progress.report;
    format!("目标：{}\n状态：{:?}{}\n回合：{}{} · 累计经过 {} 秒\n工具调用：{} · tokens：{}\n主任务恢复：{} 次 · 评估：{} 次 · 已观测等待：{} 秒\n原因：{}\n{}",
        progress.condition, progress.phase, if progress.active { "（运行中）" } else { "" }, progress.round,
        progress.max_rounds.map(|n| format!("/{n}")).unwrap_or_default(), progress.elapsed_secs,
        r.tool_calls, r.tokens, r.provider_recoveries, r.evaluator_attempts, r.waiting_secs,
        progress.last_reason.as_deref().unwrap_or("无"), if progress.evaluation_ready { "可用 /goal recheck 仅重新评估，不执行主任务。统计来自已保存检查点，中断前未保存的统计可能未计入。" } else { "续接前请核对未确认完成的操作。统计来自已保存检查点，中断前未保存的统计可能未计入。" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct Counts {
        main: AtomicUsize,
        evaluations: AtomicUsize,
        met: std::sync::atomic::AtomicBool,
    }
    struct Factory(Arc<Counts>);
    struct Provider(Arc<Counts>);
    impl CodingProviderFactory for Factory {
        fn build(
            &self,
            _: &CodingAgentConfig,
            _: Option<&str>,
        ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
            Ok(Arc::new(Provider(self.0.clone())))
        }
    }
    #[async_trait::async_trait]
    impl LlmProvider for Provider {
        fn model_name(&self) -> &str {
            "checkpoint-test"
        }
        async fn chat_stream(
            &self,
            messages: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            options: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
            let evaluation = messages.iter().any(|m| {
                m.role == atomcode_kernel::message::Role::System
                    && m.text.contains("strict goal evaluator")
            });
            let text = if evaluation {
                assert!(matches!(
                    options.tool_choice,
                    atomcode_kernel::provider::ToolChoice::None
                ));
                self.0.evaluations.fetch_add(1, Ordering::SeqCst);
                if self.0.met.load(Ordering::SeqCst) {
                    "Verdict: yes tests pass"
                } else {
                    "Verdict: no a test still fails"
                }
            } else {
                self.0.main.fetch_add(1, Ordering::SeqCst);
                "work completed"
            };
            Ok(Box::pin(futures::stream::iter([
                StreamEvent::TextDelta(text.into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 11,
                    completion: 3,
                    ..Default::default()
                }),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }
    fn input(
        root: &std::path::Path,
        counts: &Arc<Counts>,
        session: crate::SessionMode,
    ) -> CodingRuntimeStart {
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut start = super::super::tests::native_start(false);
        start.agent = CodingAgentConfig::new(
            "key",
            "https://example.test/v1",
            "checkpoint-test",
            &project,
            atomcode_capabilities::ProductDirs::new(root.join("user"), ".project"),
        );
        start.prepare.session = session;
        start.provider_factory = Arc::new(Factory(counts.clone()));
        start
    }
    async fn finished(runtime: &mut CodingRuntime) {
        tokio::time::timeout(std::time::Duration::from_secs(3600), async {
            while let Some(event) = runtime.events.recv().await {
                if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
                    return;
                }
            }
            panic!("missing turn terminal");
        })
        .await
        .unwrap();
    }
    async fn existing(
        root: &std::path::Path,
        counts: &Arc<Counts>,
    ) -> (String, Arc<atomcode_capabilities::session::SessionManager>) {
        let start = input(root, counts, crate::SessionMode::Fresh);
        let manager = Arc::new(atomcode_capabilities::session::SessionManager::for_project(
            &start.agent.working_dir,
            &start.agent.dirs,
        ));
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let id = runtime.session.as_ref().unwrap().id.clone();
        runtime
            .handle
            .submit(UserInput::from("do the work"))
            .await
            .unwrap();
        finished(&mut runtime).await;
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        (id, manager)
    }
    fn seed(
        manager: &Arc<atomcode_capabilities::session::SessionManager>,
        id: &str,
        root: &std::path::Path,
        version: u32,
        stale: bool,
    ) {
        let lease = manager.acquire_lease(id).unwrap();
        let bound = Bound {
            manager: manager.clone(),
            lease,
            path: manager.meta_path(id).unwrap(),
            working_dir: root.join("project"),
        };
        let mut state = GoalState::new(1, "tests pass".into(), 100, 0);
        state.round = 2;
        state.tokens_used = 7;
        state.report.tool_calls = 3;
        state.begin_evaluation();
        state.pause("evaluation unavailable");
        let mut marker = bound.marker().unwrap();
        if stale {
            marker.bytes += 1;
        }
        manager
            .write_controller_checkpoint(
                &bound.lease,
                &Checkpoint {
                    version,
                    saved_at_ms: atomcode_capabilities::session::now_ms(),
                    session_id: id.into(),
                    working_dir: bound.working_dir.clone(),
                    marker,
                    progress: state.progress(),
                },
            )
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn restart_restores_paused_work_rechecks_only_the_verdict_and_keeps_the_terminal_report()
    {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let (id, manager) = existing(root.path(), &counts).await;
        seed(&manager, &id, root.path(), 1, false);
        let main_before = counts.main.load(Ordering::SeqCst);
        let mut runtime = CodingRuntime::start(input(
            root.path(),
            &counts,
            crate::SessionMode::Resume(id.clone()),
        ))
        .await
        .unwrap();
        let report = runtime.handle.goal_report().await.unwrap();
        assert!(report.contains("Paused"), "{report}");
        assert_eq!(
            counts.main.load(Ordering::SeqCst),
            main_before,
            "resume must not run tools or submit work"
        );
        runtime.handle.recheck_goal().await.unwrap();
        finished(&mut runtime).await;
        let report = runtime.handle.goal_report().await.unwrap();
        assert!(report.contains("尚未达成"), "{report}");
        assert!(report.contains("tokens：21"), "{report}");
        assert_eq!(
            counts.main.load(Ordering::SeqCst),
            main_before,
            "an unmet recheck must not continue main work"
        );
        counts.met.store(true, Ordering::SeqCst);
        runtime.handle.recheck_goal().await.unwrap();
        finished(&mut runtime).await;
        assert_eq!(counts.evaluations.load(Ordering::SeqCst), 2);
        assert!(runtime
            .handle
            .goal_report()
            .await
            .unwrap()
            .contains("Satisfied"));
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let runtime = CodingRuntime::start(input(
            root.path(),
            &counts,
            crate::SessionMode::Resume(id.clone()),
        ))
        .await
        .unwrap();
        assert!(runtime
            .handle
            .goal_report()
            .await
            .unwrap()
            .contains("Satisfied"));
        assert_eq!(counts.main.load(Ordering::SeqCst), main_before);
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let lease = manager
            .acquire_lease(&id)
            .expect("shutdown releases every checkpoint lease");
        manager.delete(&lease).unwrap();
        assert!(!manager.root().join(format!("{id}.controller")).exists());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_real_goal_persists_its_terminal_report_without_replaying_on_resume() {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        counts.met.store(true, Ordering::SeqCst);
        let mut runtime =
            CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Fresh))
                .await
                .unwrap();
        let id = runtime.session.as_ref().unwrap().id.clone();
        runtime.handle.start_goal("tests pass").await.unwrap();
        finished(&mut runtime).await;
        let report = runtime.handle.goal_report().await.unwrap();
        assert!(report.contains("Satisfied"), "{report}");
        assert!(report.contains("tokens：28"), "{report}");
        assert_eq!(counts.main.load(Ordering::SeqCst), 1);
        assert_eq!(counts.evaluations.load(Ordering::SeqCst), 1);
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let runtime =
            CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Resume(id)))
                .await
                .unwrap();
        assert!(runtime
            .handle
            .goal_report()
            .await
            .unwrap()
            .contains("Satisfied"));
        assert_eq!(counts.main.load(Ordering::SeqCst), 1);
        assert_eq!(counts.evaluations.load(Ordering::SeqCst), 1);
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn observed_work_is_saved_when_a_real_native_goal_is_paused_and_resumed() {
        struct WorkProvider(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl LlmProvider for WorkProvider {
            fn model_name(&self) -> &str {
                "observed-work"
            }
            async fn chat_stream(
                &self,
                _: &[Message],
                _: &[atomcode_kernel::tool::ToolDef],
                _: &atomcode_kernel::provider::ChatOptions,
            ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError>
            {
                if self.0.fetch_add(1, Ordering::SeqCst) > 0 {
                    return std::future::pending().await;
                }
                Ok(Box::pin(futures::stream::iter([
                    StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                        id: "read-evidence".into(),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"file_path": "evidence.txt"}).to_string(),
                    }),
                    StreamEvent::Usage(TokenUsage {
                        prompt: 11,
                        completion: 3,
                        ..Default::default()
                    }),
                    StreamEvent::Done { truncated: false },
                ])))
            }
        }
        struct WorkFactory(Arc<WorkProvider>);
        impl CodingProviderFactory for WorkFactory {
            fn build(
                &self,
                _: &CodingAgentConfig,
                _: Option<&str>,
            ) -> Result<Arc<dyn LlmProvider>, crate::ProviderBuildError> {
                Ok(self.0.clone())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let provider = Arc::new(WorkProvider(Default::default()));
        let mut start = input(root.path(), &counts, crate::SessionMode::Fresh);
        std::fs::write(start.agent.working_dir.join("evidence.txt"), "evidence").unwrap();
        start.provider_factory = Arc::new(WorkFactory(provider.clone()));
        let mut runtime = CodingRuntime::start(start).await.unwrap();
        let id = runtime.session.as_ref().unwrap().id.clone();
        runtime.handle.start_goal("check evidence").await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if matches!(
                    runtime.events.recv().await.map(|e| e.event),
                    Some(CodingRuntimeEvent::Agent(AgentEvent::ToolResult { .. }))
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        runtime.handle.pause_goal().await.unwrap();
        finished(&mut runtime).await;
        let report = runtime.handle.goal_report().await.unwrap();
        assert!(report.contains("工具调用：1 · tokens：14"), "{report}");
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        let before = provider.0.load(Ordering::SeqCst);
        let mut resumed = input(root.path(), &counts, crate::SessionMode::Resume(id));
        resumed.provider_factory = Arc::new(WorkFactory(provider.clone()));
        let runtime = CodingRuntime::start(resumed).await.unwrap();
        let report = runtime.handle.goal_report().await.unwrap();
        assert!(report.contains("Paused"), "{report}");
        assert!(report.contains("工具调用：1 · tokens：14"), "{report}");
        assert_eq!(
            provider.0.load(Ordering::SeqCst),
            before,
            "restore must not replay tools"
        );
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_stale_snapshot_receipt_cannot_start_an_evaluator() {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let (id, manager) = existing(root.path(), &counts).await;
        seed(&manager, &id, root.path(), 1, false);
        let runtime =
            CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Resume(id)))
                .await
                .unwrap();
        runtime.handle.goal_report().await.unwrap();
        let mut receipt = runtime.handle.snapshot_with_revision().await.unwrap();
        receipt.revision = receipt.revision.wrapping_add(1);
        let generation = runtime_state_generation(runtime.handle.state.load(Ordering::Acquire));
        let (done, result) = oneshot::channel();
        runtime
            .handle
            .tx
            .send(CodingRuntimeControl::RecheckGoal {
                generation,
                receipt,
                done,
            })
            .unwrap();
        assert!(matches!(result.await.unwrap(), Err(RuntimeError::Busy)));
        assert_eq!(counts.evaluations.load(Ordering::SeqCst), 0);
        assert!(runtime
            .handle
            .goal_report()
            .await
            .unwrap()
            .contains("Paused"));
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stale_or_future_checkpoints_never_authorize_reevaluation_or_get_silently_overwritten()
    {
        for future in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let counts = Arc::new(Counts::default());
            let (id, manager) = existing(root.path(), &counts).await;
            seed(
                &manager,
                &id,
                root.path(),
                if future { 2 } else { 1 },
                !future,
            );
            let path = manager.root().join(format!("{id}.controller"));
            let original = std::fs::read(&path).unwrap();
            let before = counts.main.load(Ordering::SeqCst);
            let runtime =
                CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Resume(id)))
                    .await
                    .unwrap();
            if future {
                assert!(runtime.handle.goal_report().await.is_err());
                assert!(runtime.handle.start_goal("another goal").await.is_err());
            } else {
                assert!(runtime
                    .handle
                    .goal_report()
                    .await
                    .unwrap()
                    .contains("Paused"));
                assert!(runtime.handle.recheck_goal().await.is_err());
            }
            assert_eq!(counts.main.load(Ordering::SeqCst), before);
            assert_eq!(counts.evaluations.load(Ordering::SeqCst), 0);
            runtime.handle.shutdown().await.unwrap();
            runtime.task.await.unwrap();
            if future {
                assert_eq!(std::fs::read(path).unwrap(), original);
            }
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn corrupt_and_oversized_checkpoints_are_not_silently_replaced() {
        for bytes in [b"{".to_vec(), vec![b'x'; 128 * 1024 + 1]] {
            let root = tempfile::tempdir().unwrap();
            let counts = Arc::new(Counts::default());
            let (id, manager) = existing(root.path(), &counts).await;
            let path = manager.root().join(format!("{id}.controller"));
            std::fs::write(&path, &bytes).unwrap();
            let runtime =
                CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Resume(id)))
                    .await
                    .unwrap();
            assert!(runtime.handle.goal_report().await.is_err());
            assert!(runtime.handle.start_goal("do more").await.is_err());
            runtime.handle.shutdown().await.unwrap();
            runtime.task.await.unwrap();
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert_eq!(counts.evaluations.load(Ordering::SeqCst), 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_checkpoint_symlink_is_refused_without_touching_its_target() {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let (id, manager) = existing(root.path(), &counts).await;
        seed(&manager, &id, root.path(), 1, false);
        let path = manager.root().join(format!("{id}.controller"));
        let target = root.path().join("outside");
        std::fs::rename(&path, &target).unwrap();
        let bytes = std::fs::read(&target).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let runtime =
            CodingRuntime::start(input(root.path(), &counts, crate::SessionMode::Resume(id)))
                .await
                .unwrap();
        assert!(runtime.handle.goal_report().await.is_err());
        assert!(runtime.handle.start_goal("do more").await.is_err());
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
        assert_eq!(std::fs::read(target).unwrap(), bytes);
        assert_eq!(counts.evaluations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn an_unwritable_checkpoint_refuses_goal_start_before_any_model_work() {
        let root = tempfile::tempdir().unwrap();
        let counts = Arc::new(Counts::default());
        let start = input(root.path(), &counts, crate::SessionMode::Fresh);
        let manager = atomcode_capabilities::session::SessionManager::for_project(
            &start.agent.working_dir,
            &start.agent.dirs,
        );
        let runtime = CodingRuntime::start(start).await.unwrap();
        // First ask the owner, ensuring its initial absent-checkpoint read ran.
        assert!(runtime.handle.goal_report().await.is_err());
        let id = &runtime.session.as_ref().unwrap().id;
        std::fs::create_dir(manager.root().join(format!("{id}.controller"))).unwrap();
        assert!(runtime.handle.start_goal("tests pass").await.is_err());
        assert_eq!(counts.main.load(Ordering::SeqCst), 0);
        runtime.handle.shutdown().await.unwrap();
        runtime.task.await.unwrap();
    }
}

#[cfg(test)]
mod waiting_report_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelling_a_wait_records_only_observed_time_and_no_provider_attempt() {
        let mut state = GoalState::new(1, "finish".into(), 100, 0);
        state
            .schedule_recovery(GoalRecoveryKind::Provider, None)
            .unwrap();
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert_eq!(state.progress().report.waiting_secs, 5);
        state.pause("paused");
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        assert_eq!(state.progress().report.waiting_secs, 5);
        assert_eq!(state.progress().report.provider_recoveries, 0);
    }
}
