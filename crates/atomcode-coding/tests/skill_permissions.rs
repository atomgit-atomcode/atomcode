//! Skill authorization on the actual coding assembly. A risky probe records
//! execution, and all approval requests are denied to detect accidental bypass.
use async_trait::async_trait;
use atomcode_capabilities::tools::PermissionRules;
use atomcode_coding::CodingAgentConfig;
use atomcode_harness::seams::{AgentsSvc, CommandsSvc, ToolsSvc};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
mod support;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}

struct Probe(Arc<AtomicUsize>, &'static str);
#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        self.1
    }
    fn description(&self) -> &str {
        "Test-only approval probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    async fn execute(&self, args: &str, _: &ToolContext) -> ToolResult {
        let args: serde_json::Value = serde_json::from_str(args).unwrap();
        if let Some(ctx) = atomcode_harness::agent::current() {
            let session = ctx
                .require::<atomcode_harness::seams::SessionSvc>()
                .unwrap();
            let agent = ctx
                .require::<AgentsSvc>()
                .unwrap()
                .by_session(session.id())
                .unwrap();
            if args.get("steer").and_then(serde_json::Value::as_bool) == Some(true) {
                agent.send("new user input");
            }
            if args.get("cancel").and_then(serde_json::Value::as_bool) == Some(true) {
                agent.cancel();
            }
            if args.get("plan").and_then(serde_json::Value::as_bool) == Some(true) {
                ctx.require::<atomcode_harness::seams::ModesSvc>()
                    .unwrap()
                    .plan
                    .store(true, Ordering::Relaxed);
            }
        }
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: "executed".into(),
            is_error: false,
            images: vec![],
        }
    }
}

fn call(name: &str, args: serde_json::Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: args.to_string(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}
fn done() -> Vec<StreamEvent> {
    vec![
        StreamEvent::TextDelta("done".into()),
        StreamEvent::Done { truncated: false },
    ]
}
fn invoke() -> Vec<StreamEvent> {
    call("use_skill", serde_json::json!({"name":"setup"}))
}
fn probe() -> Vec<StreamEvent> {
    call("risky_probe", serde_json::json!({}))
}

async fn mount(
    root: &std::path::Path,
    allowed: &str,
    scripts: Vec<Vec<StreamEvent>>,
    deny: &[String],
) -> (support::Mounted, Arc<AtomicUsize>) {
    mount_with_key(root, "allowed-tools", allowed, scripts, deny).await
}

async fn mount_with_key(
    root: &std::path::Path,
    key: &str,
    allowed: &str,
    scripts: Vec<Vec<StreamEvent>>,
    deny: &[String],
) -> (support::Mounted, Arc<AtomicUsize>) {
    let skills = root.join("skills/setup");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(skills.join("SKILL.md"), format!("---\nname: setup\ndescription: test setup\n{key}: {allowed}\n---\nRun the requested probe.")).unwrap();
    let mut cfg = CodingAgentConfig::new(
        "k",
        "http://unused",
        "test-model",
        root,
        atomcode_coding::config::product_dirs_from_env(),
    );
    cfg.permission_rules = Arc::new(PermissionRules::parse(&[], deny).0);
    let mut opts = support::quiet_options();
    let mut dirs = vec![root.join("skills")];
    if root.join("commands").is_dir() {
        dirs.push(root.join("commands"));
    }
    opts.skill_dirs = Some(dirs);
    let mounted = support::mount(&cfg, opts, Arc::new(RecordingProvider::new(scripts))).await;
    let count = Arc::new(AtomicUsize::new(0));
    mounted
        .context()
        .require::<ToolsSvc>()
        .unwrap()
        .register(Arc::new(Probe(count.clone(), "risky_probe")))
        .unwrap();
    (mounted, count)
}

async fn collect(mounted: &mut support::Mounted) -> support::Turn {
    let mut out = support::Turn::default();
    loop {
        match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            mounted.handle.events.recv(),
        )
        .await
        .unwrap()
        .unwrap()
        {
            AgentEvent::Request { id, kind, payload } => {
                out.asked.push((kind, payload));
                mounted
                    .handle
                    .commands
                    .send(AgentCommand::Respond {
                        id,
                        value: serde_json::json!({"decision":"deny"}),
                    })
                    .unwrap();
            }
            AgentEvent::ToolResult { result } => out.tool_results.push(result),
            AgentEvent::TurnComplete { reason, .. } => {
                out.stop = Some(reason);
                return out;
            }
            AgentEvent::Error { message, .. } => out.error = Some(message),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn successful_use_skill_preauthorizes_only_its_invoking_turn() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "\n  - risky_probe",
        vec![invoke(), probe(), done(), probe(), done()],
        &[],
    )
    .await;
    let first = support::turn(&mut mounted.handle, "setup", None).await;
    assert!(first.asked.is_empty(), "{first:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let next = support::turn(&mut mounted.handle, "next task", None).await;
    assert!(
        !next.asked.is_empty(),
        "next turn must require approval: {next:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn underscore_alias_preauthorizes_only_its_turn_and_never_overrides_deny() {
    for denied in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let deny = if denied {
            vec!["risky_probe".to_owned()]
        } else {
            vec![]
        };
        let (mut mounted, count) = mount_with_key(
            root.path(),
            "allowed_tools",
            "[risky_probe]",
            vec![invoke(), probe(), done(), probe(), done()],
            &deny,
        )
        .await;
        let first = support::turn(&mut mounted.handle, "setup", None).await;
        assert!(first.asked.is_empty(), "{first:?}");
        assert_eq!(count.load(Ordering::SeqCst), usize::from(!denied));
        assert_eq!(
            first
                .tool_results
                .iter()
                .any(|r| r.content.contains("executed") && !r.is_error),
            !denied,
            "{first:?}"
        );
        let next = support::turn(&mut mounted.handle, "next task", None).await;
        assert_eq!(!next.asked.is_empty(), !denied, "{next:?}");
        assert_eq!(count.load(Ordering::SeqCst), usize::from(!denied));
        mounted.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn slash_skill_uses_the_same_authorization_path() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) =
        mount(root.path(), "[risky_probe]", vec![probe(), done()], &[]).await;
    let ctx = mounted.context();
    let agent = ctx
        .require::<AgentsSvc>()
        .unwrap()
        .list()
        .into_iter()
        .next()
        .unwrap();
    ctx.require::<CommandsSvc>()
        .unwrap()
        .find("setup", &agent)
        .unwrap()
        .run(agent, "")
        .await
        .unwrap();
    let out = collect(&mut mounted).await;
    assert!(out.asked.is_empty(), "{out:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unlisted_tools_still_ask_and_static_deny_wins() {
    for (allowed, deny, asks) in [
        ("read_file", vec![], true),
        ("risky_probe", vec!["risky_probe".to_string()], false),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (mut mounted, count) =
            mount(root.path(), allowed, vec![invoke(), probe(), done()], &deny).await;
        let out = support::turn(&mut mounted.handle, "setup", None).await;
        assert_eq!(!out.asked.is_empty(), asks, "{out:?}");
        assert!(out.tool_results.iter().any(|r| r.is_error));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        mounted.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn malformed_declarations_never_grant_a_bare_tool() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "risky_probe(command *",
        vec![invoke(), probe(), done()],
        &[],
    )
    .await;
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert!(!out.asked.is_empty());
    assert!(
        out.tool_results[0].is_error,
        "invalid skill cannot activate"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_skill_does_not_preauthorize_destructive_shell_commands() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("keep");
    std::fs::create_dir(&marker).unwrap();
    let (mut mounted, _) = mount(
        root.path(),
        "Bash",
        vec![
            invoke(),
            call("bash", serde_json::json!({"command":"rm -rf keep"})),
            done(),
        ],
        &[],
    )
    .await;
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert!(!out.asked.is_empty(), "{out:?}");
    assert!(marker.is_dir());
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn steering_revokes_a_skill_grant_within_the_same_turn() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "risky_probe",
        vec![
            invoke(),
            call("risky_probe", serde_json::json!({"steer":true})),
            probe(),
            done(),
        ],
        &[],
    )
    .await;
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert_eq!(out.asked.len(), 1, "the second probe must ask: {out:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancellation_does_not_leak_a_grant_to_the_next_turn() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "risky_probe",
        vec![
            invoke(),
            call("risky_probe", serde_json::json!({"cancel":true})),
            probe(),
            done(),
        ],
        &[],
    )
    .await;
    let first = support::turn(&mut mounted.handle, "setup", None).await;
    assert_eq!(
        first.stop,
        Some(atomcode_kernel::event::StopReason::Cancelled)
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let next = support::turn(&mut mounted.handle, "next", None).await;
    assert!(!next.asked.is_empty(), "{next:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_skill_cannot_preauthorize_a_sensitive_read() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join(".env"), "TEST_SECRET=dummy").unwrap();
    let (mut mounted, _) = mount(
        root.path(),
        "Read",
        vec![
            invoke(),
            call("read_file", serde_json::json!({"path":".env"})),
            done(),
        ],
        &[],
    )
    .await;
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert!(!out.asked.is_empty(), "{out:?}");
    assert!(out.tool_results.iter().any(|r| r.is_error));
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_skills_gateway_preserves_each_skills_permission_rules() {
    let root = tempfile::tempdir().unwrap();
    let second = root.path().join("skills/additional");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(second.join("SKILL.md"), "---\nname: additional\ndescription: additional test\nallowed-tools: risky_probe\n---\nRun the probe.").unwrap();
    let (mut mounted, count) = mount(root.path(), "Read", vec![probe(), done()], &[]).await;
    let ctx = mounted.context();
    let agent = ctx
        .require::<AgentsSvc>()
        .unwrap()
        .list()
        .into_iter()
        .next()
        .unwrap();
    ctx.require::<CommandsSvc>()
        .unwrap()
        .find("skills", &agent)
        .unwrap()
        .run(agent, "setup additional")
        .await
        .unwrap();
    let out = collect(&mut mounted).await;
    assert!(out.asked.is_empty(), "{out:?}");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn bash_patterns_match_the_actual_command_without_installing_anything() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "\n  - Bash(pip3 *)",
        vec![
            invoke(),
            call("bash", serde_json::json!({"command":"pip3 install hidapi"})),
            call("bash", serde_json::json!({"command":"pip install hidapi"})),
            done(),
        ],
        &[],
    )
    .await;
    let tools = mounted.context().require::<ToolsSvc>().unwrap();
    tools.unregister("bash");
    tools
        .register(Arc::new(Probe(count.clone(), "bash")))
        .unwrap();
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert_eq!(
        out.asked.len(),
        1,
        "only the unlisted pip command asks: {out:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_same_named_flat_command_does_not_borrow_the_skills_grant() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("commands")).unwrap();
    std::fs::write(
        root.path().join("commands/setup.md"),
        "---\nname: setup\ndescription: separate command\n---\nRun the requested probe.",
    )
    .unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "risky_probe",
        vec![probe(), done(), probe(), done()],
        &[],
    )
    .await;
    let ctx = mounted.context();
    let agent = ctx
        .require::<AgentsSvc>()
        .unwrap()
        .list()
        .into_iter()
        .next()
        .unwrap();
    let commands = ctx.require::<CommandsSvc>().unwrap();
    commands
        .find("setup", &agent)
        .unwrap()
        .run(agent.clone(), "")
        .await
        .unwrap();
    let command = collect(&mut mounted).await;
    assert_eq!(
        command.asked.len(),
        1,
        "the flat command has no grant: {command:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    commands
        .find("skills", &agent)
        .unwrap()
        .run(agent, "setup")
        .await
        .unwrap();
    let skill = collect(&mut mounted).await;
    assert!(
        skill.asked.is_empty(),
        "the skill has its own grant: {skill:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn entering_plan_mode_keeps_its_boundary_despite_an_active_skill_grant() {
    let root = tempfile::tempdir().unwrap();
    let (mut mounted, count) = mount(
        root.path(),
        "risky_probe",
        vec![
            invoke(),
            call("risky_probe", serde_json::json!({"plan":true})),
            probe(),
            done(),
        ],
        &[],
    )
    .await;
    let out = support::turn(&mut mounted.handle, "setup", None).await;
    assert!(
        out.asked.is_empty(),
        "plan mode refuses rather than asking: {out:?}"
    );
    assert!(
        out.tool_results
            .iter()
            .any(|r| r.is_error && r.content.contains("plan mode")),
        "{out:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    mounted.shutdown().await;
}
