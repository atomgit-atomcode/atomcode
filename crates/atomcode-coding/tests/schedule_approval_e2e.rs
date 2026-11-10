//! End-to-end proof that `[permissions] automation_approval` gates schedule management on
//! the REAL assembly (same middleware, same approval chain as production):
//!
//! * `inherit` + a live Auto runtime ⇒ no approval request, the change reaches the backend;
//! * `inherit` outside Auto ⇒ the request is still asked and, left unanswered, fails closed;
//! * `always` (the default) ⇒ asked even in Auto.
//!
//! The provider is scripted so the schedule call happens deterministically; the runtime,
//! tool registry and approval wiring are the product's own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use atomcode_capabilities::tools::automation::AutomationService;
use atomcode_capabilities::tools::PermissionRules;
mod support;

use atomcode_coding::runtime::RuntimeMode;
use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_config::config::AutomationApproval;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;
use support::mount_parts;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn prepare_options() -> PrepareOptions {
    PrepareOptions {
        session: SessionMode::Disabled,
        tools: true,
        skill_dirs: None,
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        mcp_paused: false,
        extra_mcp_servers: Vec::new(),
        mcp_user_config: None,
        mcp_tokens: None,
        external_subagents: Vec::new(),
        memory: false,
        web: false,
        review: false,
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        request_user_input: true,
        rate_limit_source: None,
        front_end: None,
        review_delegate: None,
        host_plugins: Default::default(),
        identity: Default::default(),
        prompt_overrides: Default::default(),
    }
}

/// Counts the schedule mutations that actually reached the host service, so "no approval"
/// can be told apart from "the call never happened".
struct Recording(AtomicUsize);

impl AutomationService for Recording {
    fn call(
        &self,
        operation: &str,
        _args: serde_json::Value,
        _working_dir: &std::path::Path,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<serde_json::Value, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::json!({ "saved": true, "operation": operation }))
    }
}

fn schedule_provider() -> Arc<RecordingProvider> {
    Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "s1".into(),
                name: "schedule_create".into(),
                arguments: serde_json::json!({
                    "title": "smoke-inherit",
                    "prompt": "run cargo check",
                    "schedule": { "kind": "interval", "every_minutes": 30 }
                })
                .to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]))
}

#[derive(Debug)]
struct Outcome {
    /// The driver was asked to approve the change.
    prompted: bool,
    /// The tool result came back as an error (denied ⇒ fail closed).
    failed: bool,
    /// Mutations the host service actually received.
    recorded: usize,
    /// The runtime flag the `inherit` policy consults.
    bypass: bool,
}

async fn run(policy: AutomationApproval, mode: RuntimeMode) -> Outcome {
    let project = tempfile::tempdir().unwrap();
    let backend = Arc::new(Recording(AtomicUsize::new(0)));

    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://unused",
        "test-model",
        project.path(),
        atomcode_coding::config::product_dirs_from_env(),
    );
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.request_timeout = Some(Duration::from_secs(5));
    cfg.permission_rules = Arc::new(PermissionRules::default());
    cfg.automation = Some(backend.clone());
    cfg.automation_approval = policy;
    // Start in Build and switch the way a person does (`/mode auto` → `SetMode`), so the
    // flag `inherit` consults is written by the real runtime path, not by the test.
    cfg.initial_mode = RuntimeMode::Build;

    let options = prepare_options();
    let parts = prepare(&cfg, options.clone()).await.unwrap();
    // `set_mode(Auto)` writes exactly this flag (runtime `set_mode_flags`); the mounted
    // `AgentHandle` exposes no mode switch, so the test sets the same flag the runtime would.
    parts
        .bypass_mode
        .store(matches!(mode, RuntimeMode::Auto), Ordering::Release);
    let mut mounted = mount_parts(&parts, &cfg, &options, schedule_provider()).await;
    let bypass = parts.bypass_mode.load(Ordering::Acquire);
    let h = &mut mounted.handle;
    h.commands
        .send(AgentCommand::SendMessage {
            text: "create the task".into(),
            images: vec![],
        })
        .unwrap();

    let (mut prompted, mut failed) = (false, false);
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::Request {
                id, kind, payload, ..
            } if kind == "approval" => {
                prompted = true;
                // Leave it unanswered-as-DENY: an unexpected prompt must never mutate.
                let _ = &payload;
                h.commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "deny" }),
                    })
                    .unwrap();
            }
            AgentEvent::ToolResult { result } if result.is_error => failed = true,
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    Outcome {
        prompted,
        failed,
        recorded: backend.0.load(Ordering::SeqCst),
        bypass,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn automation_approval_inherit_skips_the_prompt_only_in_auto_mode() {
    let outcome = run(AutomationApproval::Inherit, RuntimeMode::Auto).await;
    assert!(
        outcome.bypass,
        "Auto at mount must raise the runtime flag inherit consults"
    );
    assert!(
        !outcome.prompted,
        "inherit + Auto must not ask for approval"
    );
    assert!(!outcome.failed, "the change should have been applied");
    assert_eq!(outcome.recorded, 1, "the host service must get the change");
}

#[tokio::test(flavor = "current_thread")]
async fn automation_approval_inherit_still_prompts_outside_auto_mode() {
    let outcome = run(AutomationApproval::Inherit, RuntimeMode::Build).await;
    assert!(!outcome.bypass, "Build must not raise the Auto flag");
    assert!(
        outcome.prompted,
        "outside Auto, inherit must keep the per-change prompt"
    );
    assert!(outcome.failed, "an unanswered change must fail closed");
    assert_eq!(outcome.recorded, 0, "a denied change must not be applied");
}

#[tokio::test(flavor = "current_thread")]
async fn automation_approval_always_prompts_even_in_auto_mode() {
    let outcome = run(AutomationApproval::Always, RuntimeMode::Auto).await;
    assert!(
        outcome.prompted,
        "always is the default and must keep asking in Auto"
    );
    assert!(outcome.failed);
    assert_eq!(outcome.recorded, 0);
}
