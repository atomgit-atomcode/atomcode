//! Host-owned review handoff uses the normal request/tool seams and session log.
//! Git subprocesses use real time; no timing assertions or artificial deadlines.
use atomcode_coding::{
    CodingAgentConfig, CodingProviderFactory, CodingRuntime, CodingRuntimeStart, PrepareOptions,
    ProviderBuildError, StaticPluginHookSource,
};
use atomcode_kernel::{
    event::StopReason,
    message::Message,
    provider::{ChatOptions, LlmProvider},
    stream::{ProviderError, StreamEvent, OPEN_TIMEOUT_CODE},
    testkit::RecordingProvider,
    tool::{ToolCall, ToolDef},
};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

struct Factory(Arc<dyn LlmProvider>);

struct TimeoutOnReview(Arc<RecordingProvider>);

#[async_trait::async_trait]
impl LlmProvider for TimeoutOnReview {
    fn model_name(&self) -> &str {
        self.0.model_name()
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        if messages
            .iter()
            .any(|message| message.text.contains("=== DIFF ==="))
        {
            return Err(ProviderError {
                code: Some(OPEN_TIMEOUT_CODE.into()),
                message: "fixture timed out waiting for model response headers".into(),
                ..Default::default()
            });
        }
        self.0.chat_stream(messages, tools, options).await
    }
}
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
        _: usize,
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
async fn run(clean_repo: bool, taken_over: bool, batch: bool, timeout: bool) {
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
    if timeout {
        std::fs::write(dir.path().join("fixture.rs"), "fn fixture() {}\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "--", "fixture.rs"])
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
    let selected: Arc<dyn LlmProvider> = if timeout {
        Arc::new(TimeoutOnReview(provider.clone()))
    } else {
        provider.clone()
    };
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
        provider_factory: Arc::new(Factory(selected)),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .unwrap();
    // Auto approval must not widen the host-owned review assignment into
    // arbitrary shell commands or writes.
    runtime
        .handle
        .set_mode(atomcode_coding::RuntimeMode::Auto)
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
            if timeout {
                StopReason::Timeout
            } else if clean_repo {
                StopReason::Stopped
            } else {
                StopReason::ProviderError
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
    run(true, false, false, false).await;
}
#[tokio::test]
async fn a_failed_review_ends_without_a_retry_or_manual_fallback() {
    run(false, false, false, false).await;
}
#[tokio::test]
async fn a_persons_takeover_restores_normal_conversation() {
    run(true, true, false, false).await;
}

#[tokio::test]
async fn duplicate_review_and_unrelated_write_in_one_batch_do_not_expand_the_assignment() {
    run(true, false, true, false).await;
}

#[tokio::test]
async fn a_header_timeout_ends_as_a_timeout_without_retrying_the_assignment() {
    run(true, false, false, true).await;
}
