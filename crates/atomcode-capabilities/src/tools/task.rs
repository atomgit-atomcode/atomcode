//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const DEFAULT_MAX_CONCURRENT: usize = 3;

const EXPLORE_PERSONA: &str = "You are a READ-ONLY investigation subagent. Use read/search \
tools to answer the assigned task about the codebase. You CANNOT edit files. When done, \
stop with a concise findings report the parent agent can act on.";

const WORKER_PERSONA: &str = "You are a focused EXECUTION subagent. Do exactly the task \
described — no more, no less — honoring the working directory. Make the change, verify it \
if cheap, then stop with a one-line summary of what you changed. Do not wander outside the \
task's stated scope.";

fn default_subagent_type() -> String {
    "explore".to_string()
}

#[derive(Deserialize)]
struct SubTask {
    #[allow(dead_code)]
    description: String,
    prompt: String,
    #[serde(default = "default_subagent_type")]
    subagent_type: String,
    #[serde(default)]
    difficulty: String,
}

#[derive(Deserialize)]
struct Args {
    tasks: Vec<SubTask>,
}

pub struct TaskTool {
    make_fast_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_capable_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    make_explore_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    make_worker_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    max_concurrent: usize,
}

impl TaskTool {
    pub fn new(
        make_fast_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_capable_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_explore_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
        make_worker_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
    ) -> Self {
        Self {
            make_fast_provider: Box::new(make_fast_provider),
            make_capable_provider: Box::new(make_capable_provider),
            make_explore_tools: Box::new(make_explore_tools),
            make_worker_tools: Box::new(make_worker_tools),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }

    pub fn with_max_concurrent(mut self, n: usize) -> Self {
        self.max_concurrent = n.max(1);
        self
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }

    fn description(&self) -> &str {
        "Dispatch one or more subtasks to isolated subagents. Each task: {description, \
prompt, subagent_type: 'explore'|'worker', difficulty: 'simple'|'hard'}. 'explore' = \
read-only investigation returning findings; 'worker' = edits files then stops (you review \
the diff afterward). 'simple' runs on the fast model, 'hard' on the capable model. Give \
each worker a TIGHTLY-specified task and non-overlapping file scopes when dispatching \
several. Subagents run in parallel and cannot themselves dispatch."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {"type": "string", "description": "3-5 word label"},
                            "prompt": {"type": "string", "description": "The full subtask for the subagent"},
                            "subagent_type": {"type": "string", "enum": ["explore", "worker"]},
                            "difficulty": {"type": "string", "enum": ["simple", "hard"]}
                        },
                        "required": ["description", "prompt", "subagent_type"]
                    }
                }
            },
            "required": ["tasks"]
        })
    }

    fn risk(&self, args: &str) -> RiskLevel {
        match serde_json::from_str::<Args>(args) {
            Ok(a) if a.tasks.iter().any(|t| t.subagent_type == "worker") => RiskLevel::Risky,
            _ => RiskLevel::Safe,
        }
    }

    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        // 见 Task 4。
        ToolResult { call_id: String::new(), content: String::new(), is_error: false, images: vec![] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolRegistry;

    fn dummy() -> TaskTool {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        TaskTool::new(
            || unreachable!("provider not built in these tests"),
            || unreachable!("provider not built in these tests"),
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
    }

    #[test]
    fn name_is_task() {
        assert_eq!(dummy().name(), "task");
    }

    #[test]
    fn worker_dispatch_is_risky_explore_is_safe() {
        let t = dummy();
        let worker = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"worker"}]}"#;
        let explore = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        assert!(matches!(t.risk(worker), RiskLevel::Risky));
        assert!(matches!(t.risk(explore), RiskLevel::Safe));
    }
}
