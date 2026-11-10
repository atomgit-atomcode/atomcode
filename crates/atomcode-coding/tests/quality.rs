//! Real subprocess tests: Git and the shell use OS time, so these deliberately
//! do not use a paused Tokio clock (advancing it can expire live child processes).
use atomcode_coding::runtime::RuntimeCommands;
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeEvent,
    CodingRuntimeStart, PrepareOptions, ProviderBuildError, RuntimeMode, StaticPluginHookSource,
};
use atomcode_config::config::{QualityConfig, QualityGate};
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::{provider::LlmProvider, stream::StreamEvent, tool::ToolCall};
use std::{path::Path, sync::Arc};

struct Factory(Arc<RecordingProvider>);
impl CodingProviderFactory for Factory {
    fn build(
        &self,
        _: &CodingAgentConfig,
        _: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(self.0.clone())
    }
}
fn git(path: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("tracked.txt"), "original\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(
        dir.path(),
        &[
            "-c",
            "user.name=Quality Test",
            "-c",
            "user.email=quality@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "initial",
        ],
    );
    dir
}
fn gate(command: &str) -> QualityGate {
    QualityGate {
        name: "test".into(),
        command: command.into(),
        timeout_secs: 10,
    }
}
async fn start(
    project: &Path,
    home: &Path,
    gates: Vec<QualityGate>,
    mode: RuntimeMode,
    changed: bool,
) -> (CodingRuntime, Arc<RecordingProvider>) {
    let mut turns = vec![];
    if changed {
        turns.push(vec![
            StreamEvent::ToolCall(ToolCall {
                id: "edit".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command":"printf changed > tracked.txt"})
                    .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ]);
    }
    turns.push(vec![
        StreamEvent::TextDelta("Task handoff.".into()),
        StreamEvent::Done { truncated: false },
    ]);
    start_with_turns(project, home, gates, mode, turns).await
}

async fn start_with_turns(
    project: &Path,
    home: &Path,
    gates: Vec<QualityGate>,
    mode: RuntimeMode,
    turns: Vec<Vec<StreamEvent>>,
) -> (CodingRuntime, Arc<RecordingProvider>) {
    start_with_plugins(project, home, gates, mode, turns, Default::default()).await
}

async fn start_with_plugins(
    project: &Path,
    home: &Path,
    gates: Vec<QualityGate>,
    mode: RuntimeMode,
    turns: Vec<Vec<StreamEvent>>,
    host_plugins: atomcode_coding::HostPlugins,
) -> (CodingRuntime, Arc<RecordingProvider>) {
    let provider = Arc::new(RecordingProvider::new(turns));
    let mut config = CodingAgentConfig::new(
        "test",
        "https://example.invalid/v1",
        "test",
        project,
        atomcode_capabilities::ProductDirs::new(home, ".quality-test"),
    );
    config.permission_rules = Arc::new(
        atomcode_capabilities::tools::PermissionRules::parse(
            &[],
            &["Bash(printf quality-check *)".into()],
        )
        .0,
    );
    config.initial_mode = mode;
    config.interactive = true;
    config.quality = QualityConfig {
        enabled: true,
        gates,
    };
    let prepare = PrepareOptions {
        memory: false,
        mcp: false,
        web: false,
        review: false,
        skill_dirs: Some(vec![]),
        host_plugins,
        ..Default::default()
    };
    let runtime = CodingRuntime::start(CodingRuntimeStart {
        agent: config,
        prepare,
        provider_factory: Arc::new(Factory(provider.clone())),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .unwrap();
    (runtime, provider)
}
async fn finish(
    runtime: &mut CodingRuntime,
    deny_quality: bool,
) -> atomcode_kernel::event::StopReason {
    runtime
        .handle
        .submit("Make the change.".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), runtime.events.recv())
            .await
            .unwrap()
            .unwrap();
        match event.event {
            CodingRuntimeEvent::TurnFinished(
                atomcode_coding::TurnCompletion::Completed { reason, .. }
                | atomcode_coding::TurnCompletion::SnapshotUnavailable { reason, .. },
            ) => return reason,
            CodingRuntimeEvent::Request(request) => {
                let decision =
                    if deny_quality && request.payload.to_string().contains("quality-check") {
                        "deny"
                    } else {
                        "allow"
                    };
                runtime
                    .handle
                    .respond(request.id, serde_json::json!({"decision":decision}))
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
}
#[tokio::test]
async fn configured_checks_run_after_edits_without_another_model_call_and_become_stale() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("already-dirty.txt"), "user draft").unwrap();
    let (mut runtime, provider) = start(
        project.path(),
        home.path(),
        vec![gate("test \"$(cat tracked.txt)\" = changed")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    let status = runtime.handle.quality("status".into()).await.unwrap();
    assert!(status.contains("Quality Passed"), "{status}");
    assert_eq!(
        provider.recorded().len(),
        2,
        "verification must not ask the model to certify itself"
    );
    let evidence: String = runtime.handle.quality("evidence".into()).await.unwrap();
    assert!(evidence.contains("\"exit_code\": 0"), "{evidence}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("already-dirty.txt")).unwrap(),
        "user draft"
    );
    std::fs::write(project.path().join("tracked.txt"), "later edit").unwrap();
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality Stale"));
    assert!(runtime.handle.start_goal("goal").await.is_err());
    assert!(runtime.handle.start_loop("loop", None).await.is_err());
    runtime.handle.shutdown().await.unwrap();
}
#[tokio::test]
async fn failed_check_retains_files_and_never_claims_pass() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("exit 7")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    assert_eq!(
        finish(&mut runtime, false).await,
        atomcode_kernel::event::StopReason::PolicyDenied,
        "local check failures must not be reported as model service failures"
    );
    let status = runtime.handle.quality("status".into()).await.unwrap();
    assert!(status.contains("Quality Failed"), "{status}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "changed"
    );
    runtime.handle.shutdown().await.unwrap();
}
#[tokio::test]
async fn unchanged_task_does_not_run_configured_commands() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("touch should-not-exist")],
        RuntimeMode::Auto,
        false,
    )
    .await;
    finish(&mut runtime, false).await;
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality NotRequired"));
    assert!(!project.path().join("should-not-exist").exists());
    runtime.handle.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_check_that_changes_the_workspace_cannot_certify_it() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("printf formatted > tracked.txt")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Files changed while checks"));
    runtime.handle.shutdown().await.unwrap();
}
#[tokio::test]
async fn quality_is_opt_in_and_rejects_empty_configuration() {
    let control = atomcode_coding::quality::QualityControl::new(QualityConfig::default());
    assert!(!control.enabled());
    assert!(control.command("on", Path::new(".")).await.is_err());
}

#[tokio::test]
async fn permission_denial_never_runs_the_quality_command() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("printf quality-check > denied-marker")],
        RuntimeMode::Build,
        true,
    )
    .await;
    finish(&mut runtime, true).await;
    let status = runtime.handle.quality("status".into()).await.unwrap();
    assert!(status.contains("Quality Failed"), "{status}");
    assert!(!project.path().join("denied-marker").exists());
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_during_a_check_is_not_a_pass_and_policy_cannot_change_mid_turn() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("sleep 30")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    runtime
        .handle
        .submit("Make the change.".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::ToolStarted { call }) =
            event.event
        {
            if call.id.starts_with("quality_") {
                break;
            }
        }
    }
    let inspecting = runtime.handle.quality("evidence".into()).await.unwrap();
    let in_flight: serde_json::Value =
        serde_json::from_str(&inspecting[inspecting.find('{').unwrap()..]).unwrap();
    assert_eq!(in_flight["status"], "verifying");
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(in_flight["evidence_file"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["status"], "verifying",
        "inspection must not treat this live check as an interrupted process"
    );
    assert!(runtime.handle.quality("off".into()).await.is_err());
    runtime.handle.cancel().await.unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
            break;
        }
    }
    let status = runtime.handle.quality("status".into()).await.unwrap();
    assert!(status.contains("Quality Cancelled"), "{status}");
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "changed"
    );
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn provider_rebuild_preserves_policy_and_changed_gate_configuration_invalidates_evidence() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    runtime.handle.quality("off".into()).await.unwrap();
    let config = runtime.handle.configuration().await.unwrap();
    runtime
        .handle
        .reassemble_provider(config.clone())
        .await
        .unwrap();
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality: off"));
    runtime.handle.quality("on".into()).await.unwrap();
    let mut config = config;
    config.quality.gates[0].command = "exit 9".into();
    runtime.handle.reprepare_config(config).await.unwrap();
    let status = runtime.handle.quality("status".into()).await.unwrap();
    assert!(
        status.contains("Quality: on") && status.contains("Quality Stale"),
        "{status}"
    );
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn saved_evidence_remains_available_after_session_resume() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    let text = runtime.handle.quality("evidence".into()).await.unwrap();
    let report: serde_json::Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    let session = report["session"].as_str().unwrap().to_string();
    assert!(runtime
        .handle
        .quality_for_session(Some("wrong-session".into()), "off".into())
        .await
        .is_err());
    runtime.handle.shutdown().await.unwrap();
    let (resumed, _) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        false,
    )
    .await;
    resumed.handle.resume_session(session).await.unwrap();
    let status = resumed.handle.quality("status".into()).await.unwrap();
    assert!(status.contains("Quality Passed"), "{status}");
    resumed.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_timed_out_check_cannot_pass() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let mut check = gate("sleep 30");
    check.timeout_secs = 1;
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![check],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    let evidence = runtime.handle.quality("evidence".into()).await.unwrap();
    assert!(
        evidence.contains("\"status\": \"failed\"") && evidence.contains("\"exit_code\": null"),
        "{evidence}"
    );
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn evidence_storage_failure_stops_before_model_work() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("quality-evidence"), "blocked").unwrap();
    let (mut runtime, provider) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    assert!(provider.recorded().is_empty());
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn human_waiver_is_a_separate_audited_result_and_never_a_pass() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("exit 7")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    let previous = runtime.handle.quality("evidence".into()).await.unwrap();
    let original: serde_json::Value =
        serde_json::from_str(&previous[previous.find('{').unwrap()..]).unwrap();
    let waived = runtime
        .handle
        .quality("waive known pre-existing failure; tracked separately".into())
        .await
        .unwrap();
    assert!(waived.contains("Quality Waived") && !waived.contains("Quality Passed"));
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(original["evidence_file"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        saved["status"], "failed",
        "keep the original failure evidence"
    );
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality Waived"));
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_user_input_during_verification_is_processed_and_checked_again() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let turns = vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "edit".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command":"printf changed > tracked.txt"})
                    .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("Original handoff.".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("Updated handoff.".into()),
            StreamEvent::Done { truncated: false },
        ],
    ];
    let (mut runtime, provider) = start_with_turns(
        project.path(),
        home.path(),
        vec![gate("sleep 1")],
        RuntimeMode::Auto,
        turns,
    )
    .await;
    runtime
        .handle
        .submit("Make the change.".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .unwrap()
            .unwrap();
        if let CodingRuntimeEvent::Agent(atomcode_kernel::event::AgentEvent::ToolStarted { call }) =
            event.event
        {
            if call.id.starts_with("quality_") {
                break;
            }
        }
    }
    runtime
        .handle
        .submit("Please also explain the change.".into())
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event.event, CodingRuntimeEvent::TurnFinished(_)) {
            break;
        }
    }
    assert_eq!(
        provider.recorded().len(),
        3,
        "new input must reach the model"
    );
    assert!(provider.recorded()[2]
        .0
        .iter()
        .any(|m| m.text.contains("Please also explain")));
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality Passed"));
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn snapshots_do_not_execute_repository_git_filters() {
    let project = repo();
    std::fs::write(
        project.path().join(".gitattributes"),
        "*.txt filter=probe\n",
    )
    .unwrap();
    std::fs::write(project.path().join("tracked.txt"), "dirty").unwrap();
    git(
        project.path(),
        &["config", "filter.probe.clean", "touch filter-was-run; cat"],
    );
    git(project.path(), &["config", "filter.probe.required", "true"]);
    atomcode_coding::quality::workspace(project.path())
        .await
        .unwrap();
    assert!(!project.path().join("filter-was-run").exists());
}

// Queue real input after the quality response has been accepted, but before the
// loop chooses its terminal. This deterministically exercises same-turn reopening.
struct ReopenAfterPassPlugin(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait::async_trait]
impl atomcode_plexus::Plugin for ReopenAfterPassPlugin {
    fn name(&self) -> &'static str {
        "test-reopen-after-quality"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["quality"]
    }
    async fn apply(
        &self,
        ctx: &atomcode_plexus::Context,
        _: &serde_json::Value,
    ) -> Result<(), String> {
        let context = ctx.clone();
        let passes = self.0.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::AssistantMessage>(move |message| {
            if message.tool_calls.is_empty()
                && message.text.contains("Quality Passed")
                && passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
            {
                let log = atomcode_harness::agent::scoped(&context)
                    .service::<atomcode_harness::seams::SessionSvc>()
                    .unwrap();
                let agent = context
                    .service::<atomcode_harness::seams::AgentsSvc>()
                    .unwrap()
                    .by_session(log.id())
                    .unwrap();
                agent.inbox().send_from(
                    "Also make a second change.",
                    atomcode_harness::agent::MessageOrigin::User,
                );
            }
        });
        Ok(())
    }
}

#[tokio::test]
async fn a_verified_handoff_reopened_in_the_same_turn_gets_a_fresh_quality_attempt() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let passes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let plugins = atomcode_coding::HostPlugins::new()
        .with_plugin(Arc::new(ReopenAfterPassPlugin(passes.clone())))
        .with_layer(
            atomcode_plexus::Layer::new()
                .insert(atomcode_plexus::Entry::named("test-reopen-after-quality")),
        );
    let edit = |id: &str, text: &str| {
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command":format!("printf {text} > tracked.txt")})
                    .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ]
    };
    let handoff = || {
        vec![
            StreamEvent::TextDelta("Task handoff.".into()),
            StreamEvent::Done { truncated: false },
        ]
    };
    let (mut runtime, provider) = start_with_plugins(
        project.path(),
        home.path(),
        vec![gate("test -s tracked.txt")],
        RuntimeMode::Auto,
        vec![
            edit("first", "first"),
            handoff(),
            edit("second", "second"),
            handoff(),
        ],
        plugins,
    )
    .await;
    assert_eq!(
        finish(&mut runtime, false).await,
        atomcode_kernel::event::StopReason::Stopped
    );
    assert_eq!(passes.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(provider.recorded().len(), 4);
    assert_eq!(
        std::fs::read_to_string(project.path().join("tracked.txt")).unwrap(),
        "second"
    );
    assert!(runtime
        .handle
        .quality("status".into())
        .await
        .unwrap()
        .contains("Quality Passed"));
    runtime.handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn corrupt_historical_evidence_does_not_prevent_policy_switches() {
    let project = repo();
    let home = tempfile::tempdir().unwrap();
    let (mut runtime, _) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        true,
    )
    .await;
    finish(&mut runtime, false).await;
    let text = runtime.handle.quality("evidence".into()).await.unwrap();
    let report: serde_json::Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    let session = report["session"].as_str().unwrap().to_string();
    runtime.handle.shutdown().await.unwrap();
    std::fs::write(report["evidence_file"].as_str().unwrap(), b"{broken").unwrap();
    let (mut resumed, provider) = start(
        project.path(),
        home.path(),
        vec![gate("true")],
        RuntimeMode::Auto,
        false,
    )
    .await;
    resumed.handle.resume_session(session).await.unwrap();
    assert!(resumed
        .handle
        .quality("off".into())
        .await
        .unwrap()
        .contains("Quality disabled"));
    assert!(resumed
        .handle
        .quality("on".into())
        .await
        .unwrap()
        .contains("Quality enabled"));
    resumed.handle.quality("off".into()).await.unwrap();
    assert_eq!(
        finish(&mut resumed, false).await,
        atomcode_kernel::event::StopReason::Stopped
    );
    assert_eq!(provider.recorded().len(), 1);
    resumed.handle.shutdown().await.unwrap();
}
