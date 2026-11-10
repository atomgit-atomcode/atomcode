//! Host-owned review handoff uses the normal request/tool seams and session log.
//! Git subprocesses use real time; no timing assertions or artificial deadlines.
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, StaticPluginHookSource,
};
use atomcode_kernel::{
    event::StopReason, provider::LlmProvider, stream::StreamEvent, testkit::RecordingProvider,
    tool::ToolCall,
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

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
#[derive(Debug)]
struct Delegate(AtomicBool);
#[async_trait::async_trait]
impl atomcode_review::ReviewDelegate for Delegate {
    fn report_only(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    async fn delegate(
        &self,
        _: &Path,
        _: String,
        _: Option<String>,
    ) -> Option<Result<atomcode_review::DelegatedReview, String>> {
        None
    }
}
fn git(path: &Path) {
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
}
async fn run(clean_repo: bool, taken_over: bool, batch: bool) {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    if clean_repo {
        git(dir.path());
        assert!(std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-qm",
                "init"
            ])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    let mut first = vec![StreamEvent::ToolCall(ToolCall {
        id: "review".into(),
        name: "code_review".into(),
        arguments: "{}".into(),
    })];
    if batch {
        first.push(StreamEvent::ToolCall(ToolCall {
            id: "write".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"printf unsafe > forbidden.txt"}).to_string(),
        }));
        first.push(StreamEvent::ToolCall(ToolCall {
            id: "review-again".into(),
            name: "code_review".into(),
            arguments: "{}".into(),
        }));
    }
    first.push(StreamEvent::Done { truncated: false });
    let provider = Arc::new(RecordingProvider::new(vec![
        first,
        vec![
            StreamEvent::TextDelta("ordinary conversation continues".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let delegate = Arc::new(Delegate(AtomicBool::new(!taken_over)));
    let config = CodingAgentConfig::new(
        "test",
        "https://example.invalid/v1",
        "test",
        dir.path(),
        atomcode_capabilities::ProductDirs::new(home.path(), ".handoff-test"),
    );
    let mut runtime = CodingRuntime::start(CodingRuntimeStart {
        agent: config,
        prepare: PrepareOptions {
            memory: false,
            mcp: false,
            web: false,
            skill_dirs: Some(vec![]),
            review_delegate: Some(delegate),
            ..Default::default()
        },
        provider_factory: Arc::new(Factory(provider.clone())),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .unwrap();
    runtime.handle.submit("Review once.".into()).await.unwrap();
    let reason = loop {
        let event = runtime.events.recv().await.unwrap();
        if let atomcode_coding::CodingRuntimeEvent::TurnFinished(
            atomcode_coding::TurnCompletion::Completed { reason, .. }
            | atomcode_coding::TurnCompletion::SnapshotUnavailable { reason, .. },
        ) = event.event
        {
            break reason;
        }
    };
    if taken_over {
        assert_eq!(reason, StopReason::Stopped);
        assert_eq!(provider.recorded().len(), 2);
    } else {
        assert_eq!(
            reason,
            if clean_repo {
                StopReason::Stopped
            } else {
                StopReason::PolicyDenied
            }
        );
        assert_eq!(
            provider.recorded().len(),
            1,
            "the host must return the real result without asking the wrapper to restart review"
        );
    }
    assert!(
        !dir.path().join("forbidden.txt").exists(),
        "the wrapper cannot expand into edits"
    );
    runtime.handle.shutdown().await.unwrap();
}
#[tokio::test]
async fn a_successful_review_returns_without_another_model_request() {
    run(true, false, false).await;
}
#[tokio::test]
async fn a_failed_review_ends_without_a_retry_or_manual_fallback() {
    run(false, false, false).await;
}
#[tokio::test]
async fn a_persons_takeover_restores_normal_conversation() {
    run(true, true, false).await;
}

#[tokio::test]
async fn duplicate_review_and_unrelated_write_in_one_batch_do_not_expand_the_assignment() {
    run(true, false, true).await;
}
