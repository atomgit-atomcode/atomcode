//! Model-facing observations are logged facts, not edits or approval changes.
use async_trait::async_trait;
use atomcode_coding::{CodingAgentConfig, SessionMode};
use atomcode_harness::{
    agent::{CreateAgent, OnlySession},
    seams::{AgentLoopSvc, AgentsSvc, SystemPromptSvc, ToolsSvc},
    session::{InjectionOrigin, SessionEvent},
};
use atomcode_kernel::{
    message::Role,
    stream::StreamEvent,
    testkit::RecordingProvider,
    tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult},
};
use std::{path::Path, sync::Arc};
mod support;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}
const MARKER: &str = "=== WORKTREE AT START OF USER INPUT";
fn git(root: &Path, args: &[&str]) {
    let result = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn repository(root: &Path) {
    git(root, &["init", "-q", "--template="]);
}
fn config(root: &Path, home: &Path) -> CodingAgentConfig {
    CodingAgentConfig::new(
        "test",
        "https://unused.invalid",
        "script",
        root,
        atomcode_capabilities::ProductDirs::new(home, ".worktree-test"),
    )
}
fn done() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]
}
fn action() -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: "probe".into(),
            name: "task_probe".into(),
            arguments: "{}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}
struct Probe;
#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "task_probe"
    }
    fn description(&self) -> &str {
        "Read-only test action"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "checked".into(),
            is_error: false,
            images: vec![],
        }
    }
}

// Real Git processes need real time; paused Tokio time may expire before OS I/O completes.
#[tokio::test(flavor = "current_thread")]
async fn user_inputs_observe_current_changes_once_before_requests_without_changing_the_prefix() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    repository(root.path());
    let cfg = config(root.path(), home.path());
    let provider = Arc::new(RecordingProvider::new(vec![
        done(),
        action(),
        done(),
        done(),
    ]));
    let mut mounted = support::mount(&cfg, support::quiet_options(), provider.clone()).await;
    let ctx = mounted.context();
    ctx.require::<ToolsSvc>()
        .unwrap()
        .register(Arc::new(Probe))
        .unwrap();
    let prefix = ctx.require::<SystemPromptSvc>().unwrap().render();
    assert!(
        support::turn(&mut mounted.handle, "first clean input", None)
            .await
            .error
            .is_none()
    );
    assert!(!provider.recorded()[0]
        .0
        .iter()
        .any(|m| m.text.contains(MARKER)));
    let file = root.path().join("preexisting user edit.txt");
    std::fs::write(&file, "user content").unwrap();
    assert!(
        support::turn(&mut mounted.handle, "inspect the project", None)
            .await
            .error
            .is_none()
    );
    for request in provider.recorded().iter().skip(1) {
        assert_eq!(
            request.0.iter().filter(|m| m.text.contains(MARKER)).count(),
            1
        );
        assert!(request
            .0
            .iter()
            .any(|m| m.text.contains("turn 2") && m.text.contains("ownership is unknown")));
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "user content");
    std::fs::remove_file(file).unwrap();
    support::turn(&mut mounted.handle, "clean again", None).await;
    let log = ctx.only_session().unwrap();
    let events = log.events();
    let injected: Vec<_> = events.iter().filter(|e| matches!(&e.event, SessionEvent::Injected { text, origin: InjectionOrigin::Reminder, .. } if text.contains(MARKER))).collect();
    assert_eq!(
        injected.len(),
        1,
        "one observation for the dirty user input, not each model request"
    );
    assert!(matches!(
        injected[0].event,
        SessionEvent::Injected { turn: 2, .. }
    ));
    let first_request = events
        .iter()
        .find(|e| matches!(e.event, SessionEvent::RequestHeader { turn: 2, .. }))
        .unwrap();
    assert!(injected[0].seq < first_request.seq);
    assert_eq!(ctx.require::<SystemPromptSvc>().unwrap().render(), prefix);
    assert!(provider
        .recorded()
        .iter()
        .flat_map(|r| &r.0)
        .filter(|m| m.role == Role::System)
        .all(|m| !m.text.contains(MARKER)));
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn resumed_sessions_probe_again_while_the_saved_git_snapshot_stays_frozen() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    repository(root.path());
    let cfg = config(root.path(), home.path());
    let mut opts = support::quiet_options();
    opts.session = SessionMode::Fresh;
    let mut first =
        support::mount(&cfg, opts, Arc::new(RecordingProvider::new(vec![done()]))).await;
    support::turn(&mut first.handle, "initial clean input", None).await;
    let log = first.context().only_session().unwrap();
    let id = log.id().to_string();
    let before = first
        .context()
        .require::<SystemPromptSvc>()
        .unwrap()
        .render();
    first.shutdown().await;
    std::fs::write(root.path().join("editor edit.txt"), "keep").unwrap();
    let mut opts = support::quiet_options();
    opts.session = SessionMode::Resume(id);
    let recorder = Arc::new(RecordingProvider::new(vec![done()]));
    let mut resumed = support::mount(&cfg, opts, recorder.clone()).await;
    support::turn(&mut resumed.handle, "new user input after resume", None).await;
    assert_eq!(
        resumed
            .context()
            .require::<SystemPromptSvc>()
            .unwrap()
            .render(),
        before
    );
    assert_eq!(
        recorder.recorded()[0]
            .0
            .iter()
            .filter(|m| m.text.contains(MARKER))
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("editor edit.txt")).unwrap(),
        "keep"
    );
    resumed.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn internal_turns_and_delegated_agents_do_not_repeat_the_users_workspace_probe() {
    use atomcode_harness::agent::MessageOrigin;
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    repository(root.path());
    std::fs::write(root.path().join("user edit"), "keep").unwrap();
    let cfg = config(root.path(), home.path());
    let provider = Arc::new(RecordingProvider::new(vec![done(), done(), done()]));
    let mut mounted = support::mount(&cfg, support::quiet_options(), provider.clone()).await;
    support::turn(&mut mounted.handle, "user input", None).await;
    let ctx = mounted.context();
    let agents = ctx.require::<AgentsSvc>().unwrap();
    let lead = agents
        .list()
        .into_iter()
        .find(|a| a.parent().is_none())
        .unwrap();
    lead.send_from("internal continuation", MessageOrigin::Harness);
    ctx.require::<AgentLoopSvc>().unwrap().drive(&lead).await;
    let child = agents
        .create(
            &ctx,
            CreateAgent::new()
                .id("child-worktree-probe")
                .parent(lead.session_id())
                .persist(false),
        )
        .await
        .unwrap();
    child.send("delegated work");
    ctx.require::<AgentLoopSvc>().unwrap().drive(&child).await;
    assert_eq!(
        lead.session()
            .events()
            .iter()
            .filter(
                |e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains(MARKER))
            )
            .count(),
        1
    );
    assert!(!child
        .session()
        .events()
        .iter()
        .any(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains(MARKER))));
    agents.remove(child.id());
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_new_directory_uses_its_own_observation_instead_of_the_previous_tree() {
    let clean = tempfile::tempdir().unwrap();
    let dirty = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    repository(clean.path());
    repository(dirty.path());
    std::fs::write(dirty.path().join("preexisting edit"), "preserve").unwrap();
    for (root, expects_reminder) in [(clean.path(), false), (dirty.path(), true)] {
        let provider = Arc::new(RecordingProvider::new(vec![done()]));
        let mut tree = support::mount(
            &config(root, home.path()),
            support::quiet_options(),
            provider.clone(),
        )
        .await;
        support::turn(&mut tree.handle, "work in this directory", None).await;
        assert_eq!(
            provider.recorded()[0]
                .0
                .iter()
                .any(|m| m.text.contains(MARKER)),
            expects_reminder
        );
        tree.shutdown().await;
    }
    assert_eq!(
        std::fs::read_to_string(dirty.path().join("preexisting edit")).unwrap(),
        "preserve"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn an_unknown_workspace_status_warns_but_does_not_refuse_normal_work() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    repository(root.path());
    let provider = Arc::new(RecordingProvider::new(vec![done()]));
    let mut mounted = support::mount(
        &config(root.path(), home.path()),
        support::quiet_options(),
        provider.clone(),
    )
    .await;
    // Corrupt only this disposable fixture's Git link after preparing the tree.
    std::fs::rename(root.path().join(".git"), root.path().join("saved-git")).unwrap();
    std::fs::write(root.path().join(".git"), "gitdir: missing-git-directory\n").unwrap();
    let result = support::turn(&mut mounted.handle, "explain this project", None).await;
    assert!(result.error.is_none(), "{result:?}");
    assert_eq!(result.text, "done");
    let requests = provider.recorded();
    let warning = requests[0]
        .0
        .iter()
        .find(|m| m.text.contains(MARKER))
        .unwrap();
    assert!(
        warning.text.contains("Status: unknown")
            && warning.text.contains("continue conservatively")
    );
    mounted.shutdown().await;
}
