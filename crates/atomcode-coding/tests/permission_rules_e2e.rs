//! End-to-end proof that `[permissions]` rules act on the REAL assembly: a matched `allow`
//! skips the approval prompt entirely, a matched `deny` blocks the call, and neither can be
//! reached by accident — the same command with no rules still prompts.
//!
//! The paired no-rules run is the point: without it, "no approval was requested" could just
//! mean the command was never risky to begin with.

use std::sync::Arc;
use std::time::Duration;

use atomcode_capabilities::tools::PermissionRules;
use atomcode_coding::{assemble, prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;

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
        extra_mcp_servers: Vec::new(),
        external_subagents: Vec::new(),
        memory: false,
        web: false,
        review: false,
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        request_user_input: true,
        rate_limit_source: None,
    }
}

fn bash_provider(command: &str) -> Arc<RecordingProvider> {
    Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]))
}

/// Outcome of one turn: whether the driver was asked to approve, and whether the tool
/// result came back blocked.
struct Outcome {
    prompted: bool,
    denied: bool,
}

/// Run one turn against a fresh assembly built from `rules`, auto-DENYING any approval
/// request (so an unexpected prompt cannot silently run the command).
async fn run_turn(project: &std::path::Path, command: &str, rules: PermissionRules) -> Outcome {
    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project);
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.request_timeout = Some(Duration::from_secs(5));
    cfg.permission_rules = Arc::new(rules);

    let mut parts = prepare(&cfg, prepare_options()).await.unwrap();
    let mut h = assemble(&mut parts, &cfg, bash_provider(command))
        .unwrap()
        .spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "run it".into(),
            images: vec![],
        })
        .unwrap();

    let (mut prompted, mut denied) = (false, false);
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::Request { id, kind, .. } if kind == "approval" => {
                prompted = true;
                h.commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "deny" }),
                    })
                    .unwrap();
            }
            AgentEvent::ToolResult { result } if result.is_error => denied = true,
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;
    Outcome { prompted, denied }
}

fn rules(allow: &[&str], deny: &[&str]) -> PermissionRules {
    let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
    let deny: Vec<String> = deny.iter().map(|s| s.to_string()).collect();
    let (rules, invalid) = PermissionRules::parse(&allow, &deny);
    assert!(invalid.is_empty(), "invalid rules: {invalid:?}");
    rules
}

/// A recursive `rm` is `Risky`, so it reaches `ApprovalMiddleware` and prompts. An `allow`
/// rule must make the SAME command run with no prompt at all.
#[tokio::test]
async fn allow_rule_skips_the_approval_prompt() {
    let project = tempfile::tempdir().unwrap();

    // Baseline: no rules → the destructive command prompts (and we deny it, so it stays).
    let victim = project.path().join("victim");
    std::fs::create_dir(&victim).unwrap();
    let baseline = run_turn(project.path(), "rm -rf victim", PermissionRules::default()).await;
    assert!(
        baseline.prompted,
        "baseline must prompt — otherwise this test proves nothing"
    );
    assert!(victim.exists(), "denied command must not have run");

    // With a matching allow rule → no prompt, and the command actually runs.
    let allowed = run_turn(
        project.path(),
        "rm -rf victim",
        rules(&["Bash(rm -rf *)"], &[]),
    )
    .await;
    assert!(
        !allowed.prompted,
        "an allow rule must skip the approval prompt entirely"
    );
    assert!(!victim.exists(), "the allowed command must have run");
}

/// One rule covers a whole command family — the gap the per-command "Always" grant leaves.
#[tokio::test]
async fn one_allow_rule_covers_a_command_family() {
    let project = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        let dir = project.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let out = run_turn(
            project.path(),
            &format!("rm -rf {name}"),
            rules(&["Bash(rm *)"], &[]),
        )
        .await;
        assert!(!out.prompted, "`rm -rf {name}` must not prompt");
        assert!(!dir.exists(), "`rm -rf {name}` must have run");
    }
}

/// A `deny` rule blocks a call that would otherwise have run with no prompt at all
/// (`echo` is `Safe`, so nothing else in the chain would stop it).
#[tokio::test]
async fn deny_rule_blocks_an_otherwise_safe_call() {
    let project = tempfile::tempdir().unwrap();
    let marker = project.path().join("marker.txt");
    let command = format!("echo hi > {}", marker.display());

    let out = run_turn(project.path(), &command, rules(&[], &["Bash(echo *)"])).await;
    assert!(
        !out.prompted,
        "a deny rule blocks outright, it does not ask"
    );
    assert!(out.denied, "the tool result must come back failed");
    assert!(!marker.exists(), "the denied command must not have run");
}

/// `deny` wins over `allow` even when both match.
#[tokio::test]
async fn deny_beats_allow_in_the_real_assembly() {
    let project = tempfile::tempdir().unwrap();
    let marker = project.path().join("marker.txt");
    let command = format!("echo hi > {}", marker.display());

    let out = run_turn(
        project.path(),
        &command,
        rules(&["Bash"], &["Bash(echo *)"]),
    )
    .await;
    assert!(out.denied, "deny must win over a broader allow");
    assert!(!marker.exists());
}

/// The hard floor: a broad `allow` must not pre-authorize a call whose arguments name a
/// sensitive path. Such a call falls through and still prompts (we deny it here, so the
/// command never runs — nothing under the real `~/.ssh` is touched either way).
#[tokio::test]
async fn allow_does_not_cover_a_sensitive_target() {
    let project = tempfile::tempdir().unwrap();
    let out = run_turn(
        project.path(),
        "rm -rf ~/.ssh/atomcode-permission-rule-test",
        rules(&["Bash"], &[]),
    )
    .await;
    assert!(
        out.prompted,
        "a sensitive target must still reach an approval prompt despite `allow = [\"Bash\"]`"
    );
}
