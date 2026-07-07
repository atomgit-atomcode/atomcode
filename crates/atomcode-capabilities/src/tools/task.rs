//! `task` — 把子任务派发给隔离上下文的子 agent(subagent-by-composition)。
//! 主 agent 按难度选档位(fast/capable)、按类型(explore 只读 / worker 可编辑)
//! 选子工具集。子 agent 跑在独立内核会话里,结果用 <task_result> 包回。

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond, Outcome};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{MountedTools, RiskLevel, Tool, ToolCall, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const DEFAULT_MAX_CONCURRENT: usize = 3;
/// Per-subtask wall-clock cap: a stuck/looping child is cancelled + reported as an error
/// instead of hanging the whole `task` call forever (v1's SubAgentPool had the same guard).
const DEFAULT_SUBTASK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// After a timed-out child is cancelled, how long to wait for it to unwind cooperatively
/// and hand back its partial work before we detach it and report a bare timeout.
const GRACE_AFTER_CANCEL: std::time::Duration = std::time::Duration::from_secs(5);

/// Hard-denies any child tool call that references a sensitive path (credentials, `~/.ssh`,
/// `.env`, cloud creds). Mounted on every subagent child. Unlike the parent's
/// `SensitivePathGate` — which PROMPTS — this DENIES outright, because a subagent runs
/// `AutoRespond::AllowAll`, so a prompt would just auto-approve itself. Only the file tools'
/// path args are inspected; `bash` retains the user's authority (the worker dispatch itself
/// is Risky and user-approved — the same trust as approving a bash command in the main loop).
struct DenySensitivePaths;

#[async_trait]
impl ToolMiddleware for DenySensitivePaths {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        if crate::tools::references_sensitive_path(&call.arguments) {
            return BeforeOutcome::deny(format!(
                "subagent may not touch sensitive paths (credentials / ~/.ssh / .env): {}",
                call.name
            ));
        }
        BeforeOutcome::Proceed
    }
}

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
    subtask_timeout: std::time::Duration,
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
            subtask_timeout: DEFAULT_SUBTASK_TIMEOUT,
        }
    }

    /// Override the per-subtask wall-clock timeout (default 300s). A subtask that exceeds it
    /// is cancelled and reported as a `<task_error>` — one stuck child can't hang the batch.
    pub fn with_subtask_timeout(mut self, d: std::time::Duration) -> Self {
        self.subtask_timeout = d;
        self
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
        // Use the SAME repair-aware parse as `execute` so a `worker` dispatch with
        // control-char args is still detected as Risky (not silently downgraded to
        // Safe, which would let a file-editing worker skip the approval gate).
        match parse_task_args(args) {
            Ok(a) if a.tasks.iter().any(|t| t.subagent_type == "worker") => RiskLevel::Risky,
            _ => RiskLevel::Safe,
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let parsed: Args = match parse_task_args(args) {
            Ok(a) => a,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid task args: {e}"),
                    is_error: true,
                    images: vec![],
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
        let timeout_dur = self.subtask_timeout;
        let mut set = tokio::task::JoinSet::new();
        // Live progress: the whole batch would otherwise be a black box until every subtask
        // finishes. Emit a header + per-subtask start/done so the driver renders them live.
        ctx.progress
            .emit(format!("dispatching {} subtask(s)…", parsed.tasks.len()));

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
            // A second handle to fire the child's cancel on timeout (the token given to the
            // builder is moved in; this clone stays so we can stop a timed-out detached child).
            let cancel_on_timeout = child_cancel.clone();
            let wd = ctx.working_dir.clone();
            let label = format!(
                "{}#{}",
                if is_worker { "worker" } else { "explore" },
                idx + 1
            );
            let prompt = t.prompt;
            let desc = t.description;
            let sem = sem.clone();
            let progress = ctx.progress.clone();

            set.spawn(async move {
                let _permit = sem.acquire_owned().await.expect("semaphore not closed");
                progress.emit(format!("\u{21bb} {label} · {model}")); // ↻ started
                let child = Agent::builder()
                    .provider(provider)
                    .tools(tools)
                    .persona(persona)
                    .working_dir(wd)
                    .cancel_token(child_cancel)
                    // The child runs AutoRespond::AllowAll (no human in its loop), so the
                    // parent's prompting sensitive-path gate wouldn't protect it. Hard-deny
                    // sensitive-path file ops instead (#1).
                    .middleware(Arc::new(DenySensitivePaths))
                    .build();
                // DETACH: inner spawn lets the child run independent of this future;
                // cancel propagates only via the child_token.
                //
                // NOTE: under `panic = "abort"` (workspace default), a child panic aborts
                // the whole process before the JoinError can surface, so the join-Err arm
                // below cannot fire from a panic. Defensive parity with parallel_edit.rs.
                // `&mut handle` so a timeout doesn't drop (detach) the handle — we may need to
                // re-await it below to recover the child's partial work.
                let mut handle = tokio::spawn(async move {
                    child.run_to_completion(prompt, AutoRespond::AllowAll).await
                });
                let timed_out_msg =
                    || format!("subagent exceeded the {}s time limit", timeout_dur.as_secs());
                let outcome = match tokio::time::timeout(timeout_dur, &mut handle).await {
                    Ok(Ok(o)) => o,
                    Ok(Err(join_err)) => Outcome {
                        stop: StopReason::ProviderError,
                        error: Some(format!("subagent task crashed: {join_err}")),
                        ..Default::default()
                    },
                    Err(_elapsed) => {
                        // Wall-clock cap hit (#2). Cancel the child so it stops, then give it a
                        // brief grace window to unwind and hand back whatever partial work it did
                        // — a worker that edited files before wedging is not a total loss, and the
                        // renderer's error branch surfaces that partial output (mirrors the
                        // kernel's own stream-timeout path, which also preserves it).
                        cancel_on_timeout.cancel();
                        match tokio::time::timeout(GRACE_AFTER_CANCEL, &mut handle).await {
                            Ok(Ok(mut o)) => {
                                // Re-label as a timeout regardless of how the child reported its
                                // own cancel, so the block shows "(Timeout)" with its partial text.
                                o.stop = StopReason::Timeout;
                                o.error.get_or_insert_with(timed_out_msg);
                                o
                            }
                            // Child didn't unwind in the grace window (or join error) → detach it
                            // and report the timeout with no partial output.
                            _ => Outcome {
                                stop: StopReason::Timeout,
                                error: Some(timed_out_msg()),
                                ..Default::default()
                            },
                        }
                    }
                };
                let icon = if outcome.stop == StopReason::Stopped {
                    "\u{2713} done"
                } else {
                    "\u{2717} failed"
                };
                progress.emit(format!("{icon} · {label} · {model}"));
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

        let n_total = collected.len();
        let mut n_error = 0usize;
        let mut blocks: Vec<String> = Vec::new();
        for (label, desc, model, outcome) in collected {
            let is_err = outcome.stop != StopReason::Stopped;
            if is_err {
                n_error += 1;
            }
            // Collect any output the child produced (assistant text, else tool results).
            let produced = if !outcome.text.is_empty() {
                outcome.text
            } else {
                outcome
                    .tool_results
                    .iter()
                    .map(|r| r.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let (state, tag, body) = if is_err {
                // Preserve partial output on a bounded/failed stop (MaxRounds/Timeout/…) —
                // a worker that did real work before hitting a limit is not a total loss (#2).
                let mut b = format!("subagent stopped early ({:?})", outcome.stop);
                if let Some(e) = &outcome.error {
                    b.push_str(&format!(": {e}"));
                }
                if !produced.is_empty() {
                    b.push_str(&format!("\n--- partial output ---\n{produced}"));
                }
                ("error", "task_error", b)
            } else {
                ("completed", "task_result", produced)
            };
            blocks.push(render_task_block(&label, &desc, &model, state, tag, &body));
        }

        ToolResult {
            call_id: String::new(),
            content: blocks.join("\n"),
            // Fail the whole tool call only when EVERY subtask failed. A partial failure is
            // conveyed per-block (<task_error>/<task_result>), so the parent can act on the
            // survivors instead of re-dispatching — and double-applying — the whole batch (#5).
            is_error: n_total > 0 && n_error == n_total,
            images: vec![],
        }
    }
}

/// Parse the tool args, repairing unescaped control characters on failure (weak
/// models / gateways sometimes emit a raw newline inside a JSON string value, which
/// serde rejects). Repairs ONLY on failure, so valid JSON is never altered. Shared by
/// `risk` and `execute` so both agree on whether a dispatch contains a `worker` — a
/// mismatch would let a file-editing worker with control-char args skip the approval
/// gate.
fn parse_task_args(args: &str) -> Result<Args, serde_json::Error> {
    match serde_json::from_str::<Args>(args) {
        Ok(a) => Ok(a),
        Err(_) => serde_json::from_str::<Args>(&super::repair::repair_json(args)),
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

    /// A child whose stream never yields must be capped by the per-subtask
    /// timeout — one stuck subtask cannot hang the whole batch.
    #[tokio::test]
    async fn hanging_subtask_hits_timeout_not_batch() {
        struct HangProvider;
        #[async_trait]
        impl LlmProvider for HangProvider {
            fn model_name(&self) -> &str {
                "hang"
            }
            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                // Stream that never yields → child blocks until its cancel fires.
                Ok(stream::pending::<StreamEvent>().boxed())
            }
        }
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(HangProvider) as Arc<dyn LlmProvider>,
            || Arc::new(HangProvider) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
        .with_subtask_timeout(std::time::Duration::from_millis(150));
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        // Outer guard: if the per-subtask timeout is broken, this rejects instead of hanging CI.
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), tool.execute(args, &ctx()))
            .await
            .expect("execute must return via the per-subtask timeout, not hang");
        assert!(out.is_error, "expected timeout error, got: {}", out.content);
        assert!(
            out.content.contains("time limit"),
            "should report the time limit: {}",
            out.content
        );
    }

    /// A child that produced real output before wedging must keep that partial work
    /// in its `<task_error>` block after a timeout — not report a bare time-limit.
    #[tokio::test]
    async fn timed_out_subtask_preserves_partial_output() {
        struct PartialThenHangProvider;
        #[async_trait]
        impl LlmProvider for PartialThenHangProvider {
            fn model_name(&self) -> &str {
                "partial"
            }
            async fn chat_stream(
                &self,
                _m: &[Message],
                _t: &[ToolDef],
                _o: &ChatOptions,
            ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
                // Emit some text, then hang (no Done) → the child accumulates the text,
                // then waits forever until its cancel fires on timeout.
                let evs = stream::once(async { StreamEvent::TextDelta("PARTIAL-EDIT-DONE".into()) })
                    .chain(stream::pending());
                Ok(evs.boxed())
            }
        }
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(
            || Arc::new(PartialThenHangProvider) as Arc<dyn LlmProvider>,
            || Arc::new(PartialThenHangProvider) as Arc<dyn LlmProvider>,
            move || r1.mount(&[]),
            move || r2.mount(&[]),
        )
        .with_subtask_timeout(std::time::Duration::from_millis(150));
        let args = r#"{"tasks":[{"description":"x","prompt":"p","subagent_type":"explore"}]}"#;
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), tool.execute(args, &ctx()))
            .await
            .expect("execute must return via the per-subtask timeout, not hang");
        assert!(out.is_error, "expected timeout error, got: {}", out.content);
        assert!(out.content.contains("time limit"), "missing time limit: {}", out.content);
        assert!(
            out.content.contains("PARTIAL-EDIT-DONE"),
            "partial output must survive the timeout: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn partial_batch_failure_is_not_overall_error() {
        // 2 subtasks, one succeeds + one fails ⇒ overall is_error=false (survivors are
        // actionable), but both a <task_result> and a <task_error> appear (#5).
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mk = {
            let calls = calls.clone();
            move || {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let reply = if n == 0 { Some("did it".to_string()) } else { None };
                Arc::new(MockProvider { reply }) as Arc<dyn LlmProvider>
            }
        };
        let reg = Arc::new(ToolRegistry::new());
        let r1 = reg.clone();
        let r2 = reg.clone();
        let tool = TaskTool::new(mk.clone(), mk, move || r1.mount(&[]), move || r2.mount(&[]));
        let args = r#"{"tasks":[{"description":"a","prompt":"p","subagent_type":"explore"},{"description":"b","prompt":"q","subagent_type":"explore"}]}"#;
        let out = tool.execute(args, &ctx()).await;
        assert!(!out.is_error, "partial failure must not be overall error: {}", out.content);
        assert!(out.content.contains("<task_result>"), "missing success block: {}", out.content);
        assert!(out.content.contains("<task_error>"), "missing failure block: {}", out.content);
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

    #[test]
    fn worker_with_control_char_args_still_risky() {
        // A worker dispatch whose args carry a raw control char must NOT be downgraded
        // to Safe (which would skip the approval gate while execute() repairs + spawns).
        let worker = "{\"tasks\":[{\"description\":\"d\",\"prompt\":\"a\nb\",\"subagent_type\":\"worker\"}]}";
        assert!(
            serde_json::from_str::<serde_json::Value>(worker).is_err(),
            "test premise: raw control char must be invalid JSON"
        );
        assert!(matches!(dummy().risk(worker), RiskLevel::Risky));
    }
}
