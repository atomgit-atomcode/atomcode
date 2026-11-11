//! Drive the production skills-host row with a scripted provider. No real model
//! calls, user sessions, shell commands, or network services participate.
use async_trait::async_trait;
use atomcode_coding::{CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_harness::{
    agent::OnlySession,
    seams::{SessionSvc, ToolsSvc},
    session::{InjectionOrigin, SessionEvent},
};
use atomcode_kernel::{
    stream::StreamEvent,
    testkit::RecordingProvider,
    tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult},
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
mod support;

#[ctor::ctor]
fn isolate_home() {
    atomcode_kernel::test_support::isolate_home();
}
const BODY: &str = "AUTO_DEBUG_EVIDENCE: reproduce, inspect evidence, then make a minimal fix.";
struct Probe {
    count: Arc<AtomicUsize>,
    risky: bool,
}
#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "auto_skill_probe"
    }
    fn description(&self) -> &str {
        "Test-only check of first-action context"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn risk(&self, _: &str) -> RiskLevel {
        if self.risky {
            RiskLevel::Risky
        } else {
            RiskLevel::Safe
        }
    }
    async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
        let ctx = atomcode_harness::agent::current().unwrap();
        let log = ctx.require::<SessionSvc>().unwrap();
        assert!(
            log.derive_messages().iter().any(|m| m.text.contains(BODY)),
            "body must precede the first task action"
        );
        self.count.fetch_add(1, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: "checked".into(),
            is_error: false,
            images: vec![],
        }
    }
}
fn setup(root: &std::path::Path, metadata: &str) -> (CodingAgentConfig, PrepareOptions) {
    let skill = root.join("skills/systematic-debugging");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), format!("---\nname: systematic-debugging\ndescription: Investigate bugs and test failures\n{metadata}\nallowed-tools: [auto_skill_probe]\n---\n{BODY}")).unwrap();
    let cfg = CodingAgentConfig::new(
        "test",
        "https://unused.invalid",
        "mock",
        root,
        atomcode_coding::config::product_dirs_from_env(),
    );
    let mut opts = support::quiet_options();
    opts.skill_dirs = Some(vec![root.join("skills")]);
    (cfg, opts)
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
            name: "auto_skill_probe".into(),
            arguments: "{}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn five_implicit_reports_have_the_workflow_before_the_first_action_without_duplicate_bodies()
{
    let root = tempfile::tempdir().unwrap();
    let (cfg, opts) = setup(root.path(), "auto-load: debugging");
    let reports = [
        "测试失败了，帮我查原因并修好",
        "第一次重试等了200ms，是不是算错了",
        "修复后仍然报错，请排查",
        "Why does this test fail with an error?",
        "Fix this regression",
    ];
    let provider = Arc::new(
        RecordingProvider::new(reports.iter().flat_map(|_| [action(), done()]).collect())
            .with_ctx_window(100_000),
    );
    let mut mounted = support::mount(&cfg, opts, provider.clone()).await;
    let count = Arc::new(AtomicUsize::new(0));
    mounted
        .context()
        .require::<ToolsSvc>()
        .unwrap()
        .register(Arc::new(Probe {
            count: count.clone(),
            risky: false,
        }))
        .unwrap();
    for report in reports {
        let result = support::turn(&mut mounted.handle, report, None).await;
        assert!(result.error.is_none(), "{result:?}");
    }
    assert_eq!(count.load(Ordering::SeqCst), 5);
    assert!(provider.recorded().iter().all(|(messages, _, _)| messages
        .iter()
        .filter(|m| m.text.contains(BODY))
        .count()
        == 1));
    let events = mounted.context().only_session().unwrap().events();
    assert_eq!(events.iter().filter(|e| matches!(&e.event, SessionEvent::Injected { origin: InjectionOrigin::Reminder, text, .. } if text.contains(BODY))).count(), 1);
    assert!(!events.iter().any(|e| matches!(&e.event, SessionEvent::AssistantMessage { tool_calls, .. } if tool_calls.iter().any(|call| call.name == "use_skill"))));
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn automatic_context_does_not_activate_a_skills_allowed_tools() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, opts) = setup(root.path(), "auto-load: debugging");
    let provider = Arc::new(RecordingProvider::new(vec![action(), done()]));
    let mut mounted = support::mount(&cfg, opts, provider).await;
    let count = Arc::new(AtomicUsize::new(0));
    mounted
        .context()
        .require::<ToolsSvc>()
        .unwrap()
        .register(Arc::new(Probe {
            count: count.clone(),
            risky: true,
        }))
        .unwrap();
    let result = support::turn(&mut mounted.handle, "修复这个bug", None).await;
    assert!(
        !result.asked.is_empty(),
        "automatic loading must not bypass approval: {result:?}"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn simple_tasks_opt_out_and_dynamic_skills_do_not_auto_inject() {
    for (prompt, metadata, dynamic) in [
        ("重命名单个文件", "auto-load: debugging", false),
        ("不要用技能，修复这个bug", "auto-load: debugging", false),
        ("修复这个bug", "auto-load: off", false),
        ("修复这个bug", "auto-load: debugging", true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let (cfg, opts) = setup(root.path(), metadata);
        let sentinel = root.path().join("never-created");
        if dynamic {
            let path = root.path().join("skills/systematic-debugging/SKILL.md");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                path,
                format!("{text}\n!`echo executed > {}`", sentinel.display()),
            )
            .unwrap();
        }
        let provider = Arc::new(RecordingProvider::new(vec![done()]));
        let mut mounted = support::mount(&cfg, opts, provider).await;
        support::turn(&mut mounted.handle, prompt, None).await;
        let events = mounted.context().only_session().unwrap().events();
        assert!(!events.iter().any(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("Automatically loaded static skill:"))));
        assert!(!sentinel.exists());
        mounted.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_resumed_session_reuses_its_logged_static_workflow() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, mut opts) = setup(root.path(), "auto-load: debugging");
    opts.session = SessionMode::Fresh;
    let provider = Arc::new(RecordingProvider::new(vec![done(), done()]));
    let mut mounted = support::mount(&cfg, opts.clone(), provider.clone()).await;
    support::turn(&mut mounted.handle, "修复这个bug", None).await;
    let id = mounted.context().only_session().unwrap().id().to_string();
    mounted.shutdown().await;
    opts.session = SessionMode::Resume(id);
    let mut resumed = support::mount(&cfg, opts, provider.clone()).await;
    support::turn(&mut resumed.handle, "修复后仍然报错，请排查", None).await;
    let events = resumed.context().only_session().unwrap().events();
    assert_eq!(events.iter().filter(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("Automatically loaded static skill:"))).count(), 1);
    assert!(provider
        .recorded()
        .last()
        .unwrap()
        .0
        .iter()
        .any(|m| m.text.contains(BODY)));
    resumed.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn the_host_can_disable_automatic_loading_without_removing_manual_skills() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, mut opts) = setup(root.path(), "auto-load: debugging");
    opts.host_plugins = opts.host_plugins.with_layer(
        atomcode_coding::plexus::Layer::from_toml(
            "[[patch]]\nid = \"skills\"\nconfig = { auto_load = false }",
        )
        .unwrap(),
    );
    let provider = Arc::new(RecordingProvider::new(vec![done()]));
    let mut mounted = support::mount(&cfg, opts, provider).await;
    support::turn(&mut mounted.handle, "修复这个bug", None).await;
    let events = mounted.context().only_session().unwrap().events();
    assert!(!events.iter().any(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("Automatically loaded static skill:"))));
    assert!(mounted
        .context()
        .require::<ToolsSvc>()
        .unwrap()
        .get("use_skill")
        .is_some());
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_workflow_folded_out_by_compaction_can_be_loaded_again() {
    let root = tempfile::tempdir().unwrap();
    let (cfg, opts) = setup(root.path(), "auto-load: debugging");
    let provider = Arc::new(RecordingProvider::new(vec![done(), done()]));
    let mut mounted = support::mount(&cfg, opts, provider.clone()).await;
    support::turn(&mut mounted.handle, "修复这个bug", None).await;
    let ctx = mounted.context();
    let log = ctx.only_session().unwrap();
    let through = log.events().last().unwrap().seq;
    atomcode_harness::session::commit(
        &ctx,
        &log,
        SessionEvent::Compacted {
            turn: 1,
            through,
            from: 0,
            summary: "Earlier work was summarized.".into(),
        },
    );
    assert!(!log.derive_messages().iter().any(|m| m.text.contains(BODY)));
    support::turn(&mut mounted.handle, "修复后仍然报错，请排查", None).await;
    assert_eq!(log.events().iter().filter(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains("Automatically loaded static skill:"))).count(), 2);
    assert_eq!(
        provider
            .recorded()
            .last()
            .unwrap()
            .0
            .iter()
            .filter(|m| m.text.contains(BODY))
            .count(),
        1
    );
    drop(ctx);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_shared_registry_does_not_share_another_agents_loaded_state() {
    use atomcode_harness::{
        agent::CreateAgent,
        seams::{AgentLoopSvc, AgentsSvc},
    };
    let root = tempfile::tempdir().unwrap();
    let (cfg, opts) = setup(root.path(), "auto-load: debugging");
    let provider = Arc::new(RecordingProvider::new(vec![done(), done()]));
    let mut mounted = support::mount(&cfg, opts, provider.clone()).await;
    support::turn(&mut mounted.handle, "修复这个bug", None).await;
    let ctx = mounted.context();
    let agents = ctx.require::<AgentsSvc>().unwrap();
    let second = agents
        .create(
            &ctx,
            CreateAgent::new().id("isolated-auto-skill").persist(false),
        )
        .await
        .unwrap();
    second.send("测试失败了，帮我查原因并修好");
    ctx.require::<AgentLoopSvc>().unwrap().drive(&second).await;
    assert!(second
        .session()
        .events()
        .iter()
        .any(|e| matches!(&e.event, SessionEvent::Injected { text, .. } if text.contains(BODY))));
    assert!(provider
        .recorded()
        .last()
        .unwrap()
        .0
        .iter()
        .any(|m| m.text.contains(BODY)));
    agents.remove(second.id());
    drop(ctx);
    mounted.shutdown().await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn desktop_tasks_load_static_guidance_without_granting_its_allowed_tools() {
    for risky in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (cfg, opts) = setup(root.path(), "auto-load: computer-use");
        let provider = Arc::new(RecordingProvider::new(vec![action(), done()]));
        let mut mounted = support::mount(&cfg, opts, provider.clone()).await;
        let count = Arc::new(AtomicUsize::new(0));
        mounted
            .context()
            .require::<ToolsSvc>()
            .unwrap()
            .register(Arc::new(Probe {
                count: count.clone(),
                risky,
            }))
            .unwrap();
        let result =
            support::turn(&mut mounted.handle, "帮我在 Safari 窗口点击搜索按钮", None).await;
        assert!(provider
            .recorded()
            .iter()
            .any(|(messages, _, _)| messages.iter().any(|m| m.text.contains(BODY))));
        if risky {
            assert!(
                !result.asked.is_empty(),
                "desktop context must preserve approval: {result:?}"
            );
            assert_eq!(count.load(Ordering::SeqCst), 0);
        } else {
            assert!(result.error.is_none(), "{result:?}");
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
        let log = mounted.context().only_session().unwrap();
        assert!(log.events().iter().any(|e| matches!(&e.event, SessionEvent::Injected { origin: InjectionOrigin::Reminder, text, .. } if text.contains(BODY))));
        mounted.shutdown().await;
    }
}
