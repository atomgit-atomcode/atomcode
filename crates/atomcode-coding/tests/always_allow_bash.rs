//! The reported bug, as a test: pick "Always" on a bash approval once, and the NEXT bash
//! command must not ask again for the rest of the session.
//!
//! Two paths produced the same symptom and both are covered here:
//!   1. `ApprovalMiddleware` — the grant was keyed on the exact normalized command, so any
//!      command that differed by one argument re-prompted.
//!   2. `BashWorkspaceGate`'s UNRESOLVABLE class (`cd`-then-relative, `$()`, unexpanded
//!      `$var`) — it prompted every time and never recorded the answer at all, making
//!      "Always" a literal no-op for that whole shape of command.
//!
//! The sensitive-path floor is asserted too: it must keep re-prompting.

use std::sync::Arc;
use std::time::Duration;

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

/// A provider that emits `commands` as one bash tool call per round, then a final text.
fn scripted(commands: &[&str]) -> Arc<RecordingProvider> {
    let mut rounds: Vec<Vec<StreamEvent>> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: format!("c{i}"),
                    name: "bash".into(),
                    arguments: serde_json::json!({ "command": cmd }).to_string(),
                }),
                StreamEvent::Done { truncated: false },
            ]
        })
        .collect();
    rounds.push(vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]);
    Arc::new(RecordingProvider::new(rounds))
}

/// Run one turn that issues `commands` in order, answering EVERY approval with "always".
/// Returns the command text of each call that actually prompted.
async fn prompts_when_always_allowing(project: &std::path::Path, commands: &[&str]) -> Vec<String> {
    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project);
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.request_timeout = Some(Duration::from_secs(5));

    let mut parts = prepare(&cfg, prepare_options()).await.unwrap();
    let mut h = assemble(&mut parts, &cfg, scripted(commands))
        .unwrap()
        .spawn();
    h.commands
        .send(AgentCommand::SendMessage {
            text: "go".into(),
            images: vec![],
        })
        .unwrap();

    let mut prompted = Vec::new();
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::Request {
                id, kind, payload, ..
            } if kind == "approval" => {
                let args = payload
                    .get("args")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let command = serde_json::from_str::<serde_json::Value>(&args)
                    .ok()
                    .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
                    .unwrap_or(args);
                prompted.push(command);
                h.commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({ "decision": "allow_always" }),
                    })
                    .unwrap();
            }
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();
    let _ = h.task.await;
    prompted
}

/// THE reported bug. `rm -rf <name>` is Risky, so the first one prompts; after "Always" a
/// DIFFERENT `rm -rf` must run silently. Before the fix both prompted, because the grant was
/// keyed on the exact command.
#[tokio::test]
async fn always_allow_covers_the_next_bash_command() {
    let project = tempfile::tempdir().unwrap();
    for name in ["victim1", "victim2"] {
        std::fs::create_dir(project.path().join(name)).unwrap();
    }

    let prompted =
        prompts_when_always_allowing(project.path(), &["rm -rf victim1", "rm -rf victim2"]).await;

    assert_eq!(
        prompted.len(),
        1,
        "only the FIRST bash call may prompt; got prompts for {prompted:?}"
    );
    assert!(prompted[0].contains("victim1"));
    assert!(!project.path().join("victim1").exists(), "first must run");
    assert!(
        !project.path().join("victim2").exists(),
        "second must run without a second prompt"
    );
}

/// The `cd`-then-relative shape from the bug report: `BashWorkspaceGate` classifies it
/// UNRESOLVABLE and used to prompt every time WITHOUT ever recording the answer, so "Always"
/// did nothing at all. One prompt for the class is fail-closed; two is the bug.
#[tokio::test]
async fn always_allow_sticks_for_unresolvable_cd_then_relative_commands() {
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    for name in ["a", "b"] {
        std::fs::create_dir(elsewhere.path().join(name)).unwrap();
    }
    let dir = elsewhere.path().display();

    let prompted = prompts_when_always_allowing(
        project.path(),
        &[
            &format!("cd '{dir}' && rm -rf a"),
            &format!("cd '{dir}' && rm -rf b"),
        ],
    )
    .await;

    assert_eq!(
        prompted.len(),
        1,
        "\"Always\" must be remembered for the unresolvable class; got {prompted:?}"
    );
    assert!(!elsewhere.path().join("a").exists());
    assert!(
        !elsewhere.path().join("b").exists(),
        "second must run without a second prompt"
    );
}

/// The hard floor: a session-wide "Always" must never carry over to a command that names a
/// sensitive path — that one keeps asking. (Both are denied-by-running-nothing here: the
/// commands only read, and the second prompt is the assertion.)
#[tokio::test]
async fn always_allow_does_not_carry_over_to_a_sensitive_command() {
    let project = tempfile::tempdir().unwrap();
    std::fs::create_dir(project.path().join("victim")).unwrap();

    let prompted = prompts_when_always_allowing(
        project.path(),
        &[
            "rm -rf victim",
            "rm -rf ~/.ssh/atomcode-always-allow-test",
            "rm -rf ~/.ssh/atomcode-always-allow-test",
        ],
    )
    .await;

    assert!(
        prompted.len() >= 3,
        "a sensitive target must re-prompt every time despite a session-wide bash grant; \
         got {prompted:?}"
    );
}
