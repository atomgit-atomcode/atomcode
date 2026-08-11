use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_capabilities::team::{role_by_id, TeamRoleId, TeamRunId, TeamTaskSpec};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{TeamJobFactory, TeamModelFactory, TeamRunManager, TeamRunSnapshot};

const DEFAULT_WAIT_SECS: u64 = 30;
const MAX_WAIT_SECS: u64 = 300;

#[derive(Clone)]
pub struct TeamTool {
    manager: TeamRunManager,
    jobs: TeamJobFactory,
    models: TeamModelFactory,
}

impl TeamTool {
    pub fn new(manager: TeamRunManager, jobs: TeamJobFactory, models: TeamModelFactory) -> Self {
        Self {
            manager,
            jobs,
            models,
        }
    }

    fn result(content: impl Into<String>, is_error: bool) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: content.into(),
            is_error,
            images: Vec::new(),
        }
    }

    fn json_result(value: Value) -> ToolResult {
        Self::result(
            serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
            false,
        )
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum TeamArgs {
    Delegate {
        tasks: Vec<DelegateTask>,
    },
    Status {
        #[serde(default)]
        run_id: Option<String>,
    },
    Wait {
        run_id: String,
        #[serde(default = "default_wait_secs")]
        timeout_secs: u64,
    },
    Result {
        run_id: String,
    },
    Stop {
        run_id: String,
    },
}

#[derive(Deserialize)]
struct DelegateTask {
    description: String,
    prompt: String,
    role: TeamRoleId,
    #[serde(default)]
    scope: Vec<String>,
}

fn default_wait_secs() -> u64 {
    DEFAULT_WAIT_SECS
}

fn parse_args(args: &str) -> Result<TeamArgs, String> {
    serde_json::from_str(args).map_err(|error| format!("invalid team args: {error}"))
}

fn task_spec(task: DelegateTask) -> Result<TeamTaskSpec, String> {
    if task.description.trim().is_empty() || task.prompt.trim().is_empty() {
        return Err("team task description and prompt must not be empty".into());
    }
    let profile = role_by_id(task.role.as_str())
        .ok_or_else(|| format!("unknown team role: {}", task.role))?;
    Ok(TeamTaskSpec {
        description: task.description,
        prompt: task.prompt,
        role: task.role,
        permission: profile.permission,
        difficulty: profile.difficulty,
        scope: task.scope,
    })
}

fn snapshot_json(run: &TeamRunSnapshot, include_results: bool) -> Value {
    json!({
        "run_id": run.run_id,
        "generation": run.generation,
        "total": run.total,
        "completed": run.completed,
        "failed": run.failed,
        "stopped": run.stopped,
        "terminal": run.completed + run.failed + run.stopped == run.total,
        "members": run.members.iter().map(|member| json!({
            "id": member.id,
            "role": member.role,
            "status": format!("{:?}", member.status).to_ascii_lowercase(),
            "result": include_results.then_some(member.result.as_str()),
        })).collect::<Vec<_>>(),
    })
}

#[async_trait]
impl Tool for TeamTool {
    fn name(&self) -> &str {
        "team"
    }

    fn description(&self) -> &str {
        "Run and manage a persistent team of specialized child agents. Use `delegate` with one or more independent tasks, then `status`, `wait`, or `result` with the returned run_id; use `stop` to cancel a run. Roles determine read-only vs scoped-write authority and fast vs capable model routing. Worker roles require a non-empty working-directory-relative scope and cannot run Bash."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "oneOf": [
                {
                    "properties": {
                        "action": {"const": "delegate"},
                        "tasks": {
                            "type": "array", "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "description": {"type": "string"},
                                    "prompt": {"type": "string"},
                                    "role": {"type": "string", "enum": ["planner", "architect", "explorer", "implementer", "rust", "tui_ux", "reviewer", "tester", "debugger", "security", "performance", "docs_writer", "release_manager", "migration_compat"]},
                                    "scope": {"type": "array", "items": {"type": "string"}, "description": "Required for worker roles; ignored for read-only roles."}
                                },
                                "required": ["description", "prompt", "role"]
                            }
                        }
                    },
                    "required": ["action", "tasks"]
                },
                {"properties": {"action": {"const": "status"}, "run_id": {"type": "string"}}, "required": ["action"]},
                {"properties": {"action": {"const": "wait"}, "run_id": {"type": "string"}, "timeout_secs": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_SECS}}, "required": ["action", "run_id"]},
                {"properties": {"action": {"const": "result"}, "run_id": {"type": "string"}}, "required": ["action", "run_id"]},
                {"properties": {"action": {"const": "stop"}, "run_id": {"type": "string"}}, "required": ["action", "run_id"]}
            ]
        })
    }

    fn risk(&self, args: &str) -> RiskLevel {
        match parse_args(args) {
            Ok(TeamArgs::Delegate { tasks })
                if tasks.iter().any(|task| {
                    role_by_id(task.role.as_str()).is_some_and(|profile| {
                        profile.permission == atomcode_capabilities::team::TeamPermission::Worker
                    })
                }) =>
            {
                RiskLevel::Risky
            }
            _ => RiskLevel::Safe,
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed = match parse_args(args) {
            Ok(parsed) => parsed,
            Err(error) => return Self::result(error, true),
        };
        let outcome: Result<Value, String> = match parsed {
            TeamArgs::Delegate { tasks } => {
                let tasks = tasks
                    .into_iter()
                    .map(task_spec)
                    .collect::<Result<Vec<_>, _>>();
                match tasks {
                    Ok(tasks) => self
                        .manager
                        .delegate(tasks, Arc::clone(&self.jobs), Arc::clone(&self.models))
                        .await
                        .map(|run_id| json!({"run_id": run_id, "status": "running"})),
                    Err(error) => Err(error),
                }
            }
            TeamArgs::Status { run_id } => {
                let selected = run_id.as_deref().map(TeamRunId::new);
                self.manager.snapshot(selected.as_ref()).map(|snapshot| {
                    json!({"runs": snapshot.runs.iter().map(|run| snapshot_json(run, false)).collect::<Vec<_>>()})
                }).or_else(|| run_id.is_none().then(|| json!({"runs": []})))
                  .ok_or_else(|| format!("unknown team run: {}", run_id.unwrap_or_default()))
            }
            TeamArgs::Wait {
                run_id,
                timeout_secs,
            } => {
                let run_id = TeamRunId::new(run_id);
                let timeout = Duration::from_secs(timeout_secs.min(MAX_WAIT_SECS));
                tokio::select! {
                    wait = self.manager.wait(&run_id, timeout) => wait.and_then(|wait| {
                        self.manager.snapshot(Some(&run_id))
                            .and_then(|snapshot| snapshot.runs.first().cloned())
                            .map(|run| snapshot_json(&run, wait.terminal))
                            .ok_or_else(|| format!("unknown team run: {run_id}"))
                    }),
                    _ = ctx.cancel.cancelled() => Err("team wait cancelled".into()),
                }
            }
            TeamArgs::Result { run_id } => {
                let run_id = TeamRunId::new(run_id);
                self.manager
                    .snapshot(Some(&run_id))
                    .and_then(|snapshot| snapshot.runs.first().cloned())
                    .map(|run| snapshot_json(&run, true))
                    .ok_or_else(|| format!("unknown team run: {run_id}"))
            }
            TeamArgs::Stop { run_id } => {
                let run_id = TeamRunId::new(run_id);
                self.manager
                    .stop(&run_id)
                    .await
                    .map(|()| json!({"run_id": run_id, "status": "stopped"}))
            }
        };
        match outcome {
            Ok(value) => Self::json_result(value),
            Err(error) => Self::result(error, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::team::TeamPermission;
    use atomcode_kernel::tool::{ProgressSink, ToolContext};
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
            requester: None,
        }
    }

    fn tool(max_result_chars: usize) -> TeamTool {
        let manager = TeamRunManager::new(super::super::TeamRuntimeConfig {
            max_result_chars,
            cancel_grace: Duration::from_millis(1),
            ..Default::default()
        });
        manager.begin_generation(4);
        let jobs: TeamJobFactory = Arc::new(|task, cancel, _activity| {
            Box::pin(async move {
                if task.description == "block" {
                    cancel.cancelled().await;
                    return super::super::TeamMemberOutcome::failed("cancelled");
                }
                super::super::TeamMemberOutcome::completed("abcdefghij")
            })
        });
        TeamTool::new(manager, jobs, Arc::new(|_| "test-model".to_string()))
    }

    #[tokio::test]
    async fn delegate_status_wait_result_and_stop() {
        let tool = tool(5);
        let empty = tool.execute(r#"{"action":"status"}"#, &ctx()).await;
        assert!(!empty.is_error, "{}", empty.content);
        assert_eq!(
            serde_json::from_str::<Value>(&empty.content).unwrap()["runs"],
            json!([])
        );
        let delegated = tool.execute(r#"{"action":"delegate","tasks":[{"description":"read","prompt":"inspect","role":"explorer"}]}"#, &ctx()).await;
        assert!(!delegated.is_error, "{}", delegated.content);
        let run_id = serde_json::from_str::<Value>(&delegated.content).unwrap()["run_id"]
            .as_str()
            .unwrap()
            .to_string();
        let waited = tool
            .execute(
                &format!(r#"{{"action":"wait","run_id":"{run_id}","timeout_secs":1}}"#),
                &ctx(),
            )
            .await;
        assert_eq!(
            serde_json::from_str::<Value>(&waited.content).unwrap()["terminal"],
            true
        );
        assert_eq!(
            serde_json::from_str::<Value>(&waited.content).unwrap()["members"][0]["result"],
            "abcde…"
        );
        let result = tool
            .execute(
                &format!(r#"{{"action":"result","run_id":"{run_id}"}}"#),
                &ctx(),
            )
            .await;
        assert!(result.content.contains("abcde…"));

        let blocked = tool.execute(r#"{"action":"delegate","tasks":[{"description":"block","prompt":"wait","role":"explorer"}]}"#, &ctx()).await;
        let blocked_value = serde_json::from_str::<Value>(&blocked.content).unwrap();
        let blocked_id = blocked_value["run_id"].as_str().unwrap().to_string();
        let stopped = tool
            .execute(
                &format!(r#"{{"action":"stop","run_id":"{blocked_id}"}}"#),
                &ctx(),
            )
            .await;
        assert!(!stopped.is_error, "{}", stopped.content);
    }

    #[tokio::test]
    async fn malformed_unknown_and_worker_without_scope_fail_closed() {
        let tool = tool(100);
        assert!(tool.execute("{", &ctx()).await.is_error);
        assert!(
            tool.execute(r#"{"action":"status","run_id":"missing"}"#, &ctx())
                .await
                .is_error
        );
        let worker = tool.execute(r#"{"action":"delegate","tasks":[{"description":"edit","prompt":"change","role":"rust"}]}"#, &ctx()).await;
        assert!(worker.is_error);
        assert!(worker.content.contains("non-empty scope"));
        assert_eq!(tool.risk(r#"{"action":"delegate","tasks":[{"description":"edit","prompt":"change","role":"rust","scope":["src/**"]}]}"#), RiskLevel::Risky);
        assert_eq!(
            role_by_id("rust").unwrap().permission,
            TeamPermission::Worker
        );
    }
}
