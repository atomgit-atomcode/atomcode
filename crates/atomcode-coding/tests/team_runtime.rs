//! End-to-end team runtime tests (no network): a scripted `AlwaysStopProvider`
//! drives each team member through a full agent turn. Proves
//! `TeamRunnerFactory` builds a working sub-agent and `TeamRunManager` +
//! `TeamTool` delegate, wait, and aggregate its outcomes.

use std::sync::Arc;
use std::time::Duration;

use atomcode_capabilities::team::{TeamDifficulty, TeamPermission, TeamRoleId, TeamTaskSpec};
use atomcode_coding::team::{
    TeamActivitySink, TeamRunManager, TeamRunnerFactory, TeamRuntimeConfig, TeamTool,
};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::testkit::AlwaysStopProvider;
use atomcode_kernel::tool::{ProgressSink, Tool, ToolContext, ToolRegistry};
use tokio_util::sync::CancellationToken;

/// Provider factory that always stops with the given text — every difficulty
/// routes to the same scripted provider, so no network and no flaky timing.
fn always_stop_providers(
    text: &'static str,
) -> Arc<dyn Fn(TeamDifficulty) -> Arc<dyn LlmProvider> + Send + Sync> {
    Arc::new(move |_difficulty| Arc::new(AlwaysStopProvider::new(text)) as Arc<dyn LlmProvider>)
}

/// Empty tool mount — the scripted provider never calls tools.
fn empty_tools() -> Arc<dyn Fn(TeamPermission) -> atomcode_kernel::tool::MountedTools + Send + Sync>
{
    Arc::new(|_permission| ToolRegistry::new().mount(&[]))
}

fn explorer_task(prompt: &str) -> TeamTaskSpec {
    TeamTaskSpec {
        description: "inspect".into(),
        prompt: prompt.into(),
        role: TeamRoleId::Explorer,
        permission: TeamPermission::Explore,
        difficulty: TeamDifficulty::Simple,
        scope: vec![],
    }
}

fn tool_ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

/// Runner end-to-end: a single member task runs a REAL agent turn (builder →
/// provider → run_to_completion) and returns a completed outcome whose text is
/// the provider's output. The progress hook must have emitted activity.
#[tokio::test]
async fn runner_completes_a_single_member_task() {
    let runner = TeamRunnerFactory::new(
        always_stop_providers("member report"),
        empty_tools(),
        std::env::temp_dir(),
    );
    let job = runner.job_factory();

    let activity_log = Arc::new(std::sync::Mutex::new(Vec::<(String, u64)>::new()));
    let sink_log = Arc::clone(&activity_log);
    let activity: TeamActivitySink = Arc::new(move |text, tokens| {
        sink_log.lock().unwrap().push((text, tokens));
    });

    let outcome = job(
        explorer_task("produce a report"),
        CancellationToken::new(),
        activity,
    )
    .await;

    assert!(
        outcome.success,
        "member should complete, got stop={:?}",
        outcome.stop
    );
    assert!(
        outcome.output.contains("member report"),
        "output should carry the provider text: {:?}",
        outcome.output
    );
    assert!(
        !activity_log.lock().unwrap().is_empty(),
        "progress hook must publish at least one activity"
    );
    assert_eq!(outcome.stop, "completed");
}

/// Runner end-to-end with a worker role: scope is required and the persona is
/// injected, bash is denied by the middleware, yet the scripted turn still
/// completes because the provider never calls tools.
#[tokio::test]
async fn runner_worker_role_completes_with_scope() {
    let runner = TeamRunnerFactory::new(
        always_stop_providers("worker done"),
        empty_tools(),
        std::env::temp_dir(),
    );
    let job = runner.job_factory();
    let spec = TeamTaskSpec {
        description: "edit".into(),
        prompt: "make the focused change".into(),
        role: TeamRoleId::Implementer,
        permission: TeamPermission::Worker,
        difficulty: TeamDifficulty::Simple,
        scope: vec!["src/**".into()],
    };
    let outcome = job(spec, CancellationToken::new(), Arc::new(|_, _| {})).await;
    assert!(
        outcome.success,
        "worker member should complete: {:?}",
        outcome.stop
    );
    assert!(outcome.output.contains("worker done"));
}

/// Manager + tool + runner 联动：delegate 两个任务 → wait 到终态 →
/// result 聚合出 completed=2。验证全链路（TeamTool → TeamRunManager →
/// TeamRunnerFactory job）真实跑通。
#[tokio::test]
async fn team_tool_delegates_waits_and_aggregates_end_to_end() {
    let runner = TeamRunnerFactory::new(
        always_stop_providers("member done"),
        empty_tools(),
        std::env::temp_dir(),
    );
    let manager = TeamRunManager::new(TeamRuntimeConfig::default());
    manager.begin_generation(1);
    let tool = TeamTool::new(manager, runner.job_factory(), runner.model_factory());
    let ctx = tool_ctx();

    // delegate 两个独立任务。
    let delegated = tool
        .execute(
            r#"{"action":"delegate","tasks":[
                {"description":"read","prompt":"inspect a","role":"explorer"},
                {"description":"read","prompt":"inspect b","role":"explorer"}
            ]}"#,
            &ctx,
        )
        .await;
    assert!(
        !delegated.is_error,
        "delegate failed: {}",
        delegated.content
    );
    let run_id = serde_json::from_str::<serde_json::Value>(&delegated.content).unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    // wait 直到终态。
    let waited = tool
        .execute(
            &format!(r#"{{"action":"wait","run_id":"{run_id}","timeout_secs":10}}"#),
            &ctx,
        )
        .await;
    assert!(!waited.is_error, "wait failed: {}", waited.content);
    let waited_value = serde_json::from_str::<serde_json::Value>(&waited.content).unwrap();
    assert_eq!(waited_value["terminal"], true, "run should be terminal");

    // result 聚合：completed=2，两个成员都在。
    let resulted = tool
        .execute(
            &format!(r#"{{"action":"result","run_id":"{run_id}"}}"#),
            &ctx,
        )
        .await;
    assert!(!resulted.is_error, "result failed: {}", resulted.content);
    let result_value = serde_json::from_str::<serde_json::Value>(&resulted.content).unwrap();
    assert_eq!(result_value["completed"], 2, "both members completed");
    assert_eq!(result_value["failed"], 0);
    assert_eq!(result_value["members"].as_array().unwrap().len(), 2);
}

/// 并发上限：超过 max_concurrent 的成员排队，最终全部完成。
#[tokio::test]
async fn concurrent_cap_queues_and_completes_all_members() {
    let runner = TeamRunnerFactory::new(
        always_stop_providers("queued done"),
        empty_tools(),
        std::env::temp_dir(),
    );
    let manager = TeamRunManager::new(TeamRuntimeConfig {
        max_concurrent: 2,
        cancel_grace: Duration::from_millis(50),
        ..TeamRuntimeConfig::default()
    });
    manager.begin_generation(1);
    let tool = TeamTool::new(manager, runner.job_factory(), runner.model_factory());
    let ctx = tool_ctx();

    // 5 个任务，max_concurrent=2 → 3 个排队，最终全部完成。
    let delegated = tool
        .execute(
            r#"{"action":"delegate","tasks":[
                {"description":"read","prompt":"inspect 1","role":"explorer"},
                {"description":"read","prompt":"inspect 2","role":"explorer"},
                {"description":"read","prompt":"inspect 3","role":"explorer"},
                {"description":"read","prompt":"inspect 4","role":"explorer"},
                {"description":"read","prompt":"inspect 5","role":"explorer"}
            ]}"#,
            &ctx,
        )
        .await;
    assert!(
        !delegated.is_error,
        "delegate failed: {}",
        delegated.content
    );
    let run_id = serde_json::from_str::<serde_json::Value>(&delegated.content).unwrap()["run_id"]
        .as_str()
        .unwrap()
        .to_string();

    let waited = tool
        .execute(
            &format!(r#"{{"action":"wait","run_id":"{run_id}","timeout_secs":15}}"#),
            &ctx,
        )
        .await;
    assert!(!waited.is_error, "wait failed: {}", waited.content);
    let waited_value = serde_json::from_str::<serde_json::Value>(&waited.content).unwrap();
    assert_eq!(waited_value["terminal"], true);

    let resulted = tool
        .execute(
            &format!(r#"{{"action":"result","run_id":"{run_id}"}}"#),
            &ctx,
        )
        .await;
    let result_value = serde_json::from_str::<serde_json::Value>(&resulted.content).unwrap();
    assert_eq!(result_value["completed"], 5, "all queued members complete");
    assert_eq!(result_value["members"].as_array().unwrap().len(), 5);
}
