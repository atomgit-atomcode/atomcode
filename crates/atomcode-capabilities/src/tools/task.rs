//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond, Outcome};
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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(_) => {
                // Weak models / gateways sometimes emit unescaped control characters
                // inside JSON string values (e.g. a raw newline in `prompt`), which
                // serde rejects. Repair (escape control chars in strings) then retry —
                // ONLY on failure, so valid JSON is never altered. Reuses the shared
                // tool-arg JSON repairer.
                match serde_json::from_str(&super::repair::repair_json(args)) {
                    Ok(a) => a,
                    Err(e) => {
                        return ToolResult {
                            call_id: String::new(),
                            content: format!("invalid task args: {e}"),
                            is_error: true,
                            images: vec![],
                        }
                    }
                }
            }
        };
        if parsed.tasks.is_empty() {
            return ToolResult {
                call_id: String::new(),
                content: "no tasks provided".into(),
                is_error: true,
                images: vec![],
            };
        }

        let sem = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent));
        let mut set = tokio::task::JoinSet::new();

        for (idx, t) in parsed.tasks.into_iter().enumerate() {
            let is_worker = t.subagent_type == "worker";
            let is_hard = t.difficulty == "hard";
            // Fresh provider + fresh tools per child (a session consumes its provider).
            let provider = if is_hard {
                (self.make_capable_provider)()
            } else {
                (self.make_fast_provider)()
            };
            // Capture the actual model this subtask runs on (for display + routing proof)
            // BEFORE the provider is moved into the child builder.
            let model = provider.model_name().to_string();
            let tools = if is_worker {
                (self.make_worker_tools)()
            } else {
                (self.make_explore_tools)()
            };
            let persona = if is_worker { WORKER_PERSONA } else { EXPLORE_PERSONA }.to_string();
            let child_cancel = ctx.cancel.child_token();
            let wd = ctx.working_dir.clone();
            let label = format!(
                "{}#{}",
                if is_worker { "worker" } else { "explore" },
                idx + 1
            );
            let prompt = t.prompt;
            let desc = t.description;
            let sem = sem.clone();

            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                let child = Agent::builder()
                    .provider(provider)
                    .tools(tools)
                    .persona(persona)
                    .working_dir(wd)
                    .cancel_token(child_cancel)
                    .build();
                // DETACH: inner spawn lets the child run independent of this future;
                // cancel propagates only via the child_token.
                //
                // NOTE: under `panic = "abort"` (workspace default), a child panic aborts
                // the whole process before the JoinError can surface, so the Err arm below
                // cannot fire from a panic.  This is defensive parity with parallel_edit.rs:
                // it removes the silent-success footgun (Outcome::default() == Stopped) and
                // makes any future non-panic JoinError (e.g. explicit abort()) visible.
                let outcome = match tokio::spawn(async move {
                    child.run_to_completion(prompt, AutoRespond::AllowAll).await
                })
                .await
                {
                    Ok(o) => o,
                    Err(join_err) => Outcome {
                        stop: StopReason::ProviderError,
                        error: Some(format!("subagent task crashed: {join_err}")),
                        ..Default::default()
                    },
                };
                (label, desc, model, outcome)
            });
        }

        // Collect all child results (order determined by completion, then sorted by label).
        // The outer closure always returns Ok(tuple); inner JoinErrors are handled at the
        // inner spawn site above and mapped to an errored Outcome.
        let mut collected: Vec<(String, String, String, Outcome)> = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(tuple) = res {
                collected.push(tuple);
            }
        }
        // Sort by label for deterministic output regardless of scheduling order.
        collected.sort_by(|a, b| a.0.cmp(&b.0));

        let mut blocks: Vec<String> = Vec::new();
        let mut any_error = false;
        for (label, desc, model, outcome) in collected {
            let is_err = outcome.stop != StopReason::Stopped;
            any_error |= is_err;
            let (state, tag, body) = if is_err {
                (
                    "error",
                    "task_error",
                    format!(
                        "subagent failed ({:?}): {}",
                        outcome.stop,
                        outcome.error.unwrap_or_else(|| "<no error message>".into())
                    ),
                )
            } else if !outcome.text.is_empty() {
                ("completed", "task_result", outcome.text)
            } else {
                // Pure-tool child (no assistant text): fall back to tool_results.
                let joined = outcome
                    .tool_results
                    .iter()
                    .map(|r| r.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                ("completed", "task_result", joined)
            };
            blocks.push(render_task_block(&label, &desc, &model, state, tag, &body));
        }

        ToolResult {
            call_id: String::new(),
            content: blocks.join("\n"),
            is_error: any_error,
            images: vec![],
        }
    }
}

/// Wrap a child-agent result in an opencode-style `<task>` block. `model` is the
/// model the subagent actually ran on (surfaced so the user can see which tier/model
/// executed — the strong/weak routing proof).
fn render_task_block(
    id: &str,
    summary: &str,
    model: &str,
    state: &str,
    tag: &str,
    body: &str,
) -> String {
    format!(
        "<task id=\"{id}\" model=\"{model}\" state=\"{state}\">\n<summary>{summary}</summary>\n<{tag}>\n{body}\n</{tag}>\n</task>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent};
    use atomcode_kernel::tool::{ProgressSink, ToolDef, ToolRegistry};
    use futures::stream::{self, BoxStream};
    use futures::StreamExt;
    use tokio_util::sync::CancellationToken;

    /// Scripted provider: `Some(reply)` → one text turn then clean stop;
    /// `None` → a terminal open error (simulates a failed child).
    struct MockProvider {
        reply: Option<String>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn model_name(&self) -> &str {
            "mock"
        }
        async fn chat_stream(
            &self,
            _m: &[Message],
            _t: &[ToolDef],
            _o: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            match &self.reply {
                Some(text) => {
                    let evs = vec![
                        StreamEvent::TextDelta(text.clone()),
                        StreamEvent::Done { truncated: false },
                    ];
                    Ok(stream::iter(evs).boxed())
                }
                None => Err(ProviderError {
                    retryable: false,
                    message: "mock open failure".into(),
                    ..Default::default()
                }),
            }
        }
    }

    fn ctx() -> ToolContext {
        // Dedicated EMPTY tempdir — shared std::env::temp_dir() can contain stray
        // build markers that confuse any build-detection logic in child agents.
        let dir = tempfile::tempdir().expect("tempdir").keep();
        ToolContext {
            working_dir: dir,
            cancel: CancellationToken::new(),
            progress: ProgressSink::noop(),
        }
    }

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

    #[tokio::test]
    async fn explore_task_returns_task_result() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: Some("FOUND: the answer is 42".into()) }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: Some("FOUND: the answer is 42".into()) }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"find","prompt":"where is X","subagent_type":"explore","difficulty":"simple"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "unexpected error: {}", out.content);
        assert!(out.content.contains("<task_result>"), "missing tag: {}", out.content);
        assert!(out.content.contains("FOUND: the answer is 42"), "missing reply: {}", out.content);
        assert!(out.content.contains("state=\"completed\""), "missing state: {}", out.content);
    }

    #[tokio::test]
    async fn failed_child_returns_task_error() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: None }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(out.is_error, "expected error result, got: {}", out.content);
        assert!(out.content.contains("<task_error>"), "missing tag: {}", out.content);
    }

    #[tokio::test]
    async fn task_block_carries_provider_model() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: Some("done".into()) }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: Some("done".into()) }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        let args = r#"{"tasks":[{"description":"d","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        // The block surfaces the actual model the subagent ran on (MockProvider::model_name).
        assert!(out.content.contains("model=\"mock\""), "missing model attr: {}", out.content);
    }

    #[tokio::test]
    async fn control_char_in_args_is_repaired() {
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(MockProvider { reply: Some("ok".into()) }) as Arc<dyn LlmProvider>,
            || Arc::new(MockProvider { reply: Some("ok".into()) }) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        );
        // A RAW newline (0x0A) inside the `prompt` string value — serde rejects this
        // outright ("control character found"); the try-then-repair path must recover it.
        let args = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"line1\nline2\",\"subagent_type\":\"explore\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(args).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        let out = tool.execute(args, &ctx()).await;
        assert!(
            !out.content.contains("invalid task args"),
            "repair should have recovered the args, got: {}",
            out.content
        );
        assert!(out.content.contains("<task_result>"), "expected a result: {}", out.content);
    }
}
