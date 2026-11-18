//! Runtime-owned observations of `bash_start` completion requests.
//! No process ownership: cancellation forgets notifications and leaves polling available.
use atomcode_capabilities::tools::bash::{observe_completion, BackgroundCompletion};
use atomcode_kernel::tool::{ToolCall, ToolResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use std::collections::{HashMap, HashSet};

/// Output all background completions delivered in one wakeup share.
const NOTICE_BUDGET_CHARS: usize = 8000;
const NOTICE_TAIL_CHARS: usize = 2000;

pub(crate) fn completion_notice(batch: &[BackgroundCompletion], goal_waiting: bool) -> String {
    let tail = (NOTICE_BUDGET_CHARS / batch.len().max(1)).min(NOTICE_TAIL_CHARS);
    batch
        .iter()
        .filter(|completion| goal_waiting || !completion.killed)
        .filter_map(|completion| {
            completion.notification_within(tail).or_else(|| {
                // A terminal poll or store reclamation can remove the output
                // before we deliver the completion to a goal parked on it.
                goal_waiting.then(|| {
                    format!(
                        "Background job {} has finished (exit code: {:?}, killed: {}). Its output is no longer available to collect.",
                        completion.job_id, completion.exit_code, completion.killed,
                    )
                })
            })
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Default)]
pub(crate) struct BackgroundJobs {
    generation: u64,
    calls: HashSet<String>,
    pending: HashSet<String>,
    waits: FuturesUnordered<BoxFuture<'static, BackgroundCompletion>>,
}

impl BackgroundJobs {
    pub fn reset(&mut self, generation: u64) {
        *self = Self {
            generation,
            ..Default::default()
        };
    }
    pub fn retain_generation(&mut self, generation: u64) {
        if generation != self.generation {
            self.reset(generation);
        }
    }
    pub fn started(&mut self, call: &ToolCall) {
        if matches!(call.name.as_str(), "bash_start" | "pwsh_start")
            && serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .is_some_and(|args| {
                    args.get("notify_on_exit").and_then(|v| v.as_bool()) != Some(false)
                })
        {
            self.calls.insert(call.id.clone());
        }
    }
    pub fn result(&mut self, result: &ToolResult) {
        if !self.calls.remove(&result.call_id) || result.is_error {
            return;
        }
        let Some(job) = crate::discipline::started_job(&result.content) else {
            return;
        };
        if self.pending.contains(job) {
            return;
        }
        if let Some(wait) = observe_completion(job) {
            self.pending.insert(job.to_string());
            self.waits.push(wait);
        }
    }
    /// Collected jobs must not park a goal waiting for a suppressed notification.
    pub fn uncollected(&self) -> bool {
        self.pending
            .iter()
            .any(|id| observe_completion(id).is_some())
    }
    pub fn pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub async fn next(&mut self) -> BackgroundCompletion {
        // Called only when pending; a generation reset drops all outstanding waits.
        let completion = self
            .waits
            .next()
            .await
            .expect("pending job has a completion observer");
        self.pending.remove(&completion.job_id);
        completion
    }
    /// Completions that have already happened, taken without waiting, so the
    /// runtime can hand a batch to the model as one notice.
    pub fn ready(&mut self) -> Vec<BackgroundCompletion> {
        let mut ready = Vec::new();
        while let Some(Some(completion)) = self.waits.next().now_or_never() {
            self.pending.remove(&completion.job_id);
            ready.push(completion);
        }
        ready
    }
}

/// A changed result is evidence of activity, not proof of success. Identical
/// call+result pairs from earlier turns do not reset the goal's stall counter.
/// Unknown/mutating tools stay conservative: their side effects may not print.
pub(crate) fn made_progress(messages: &[atomcode_kernel::message::Message]) -> bool {
    use atomcode_kernel::message::Role;
    let start = messages
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0);
    let mut calls = HashMap::new();
    let mut earlier = HashSet::new();
    let mut progress = false;
    for (index, message) in messages.iter().enumerate() {
        for call in &message.tool_calls {
            calls.insert(call.id.as_str(), call);
        }
        if message.role != Role::Tool {
            continue;
        }
        let Some(call) = message.tool_call_id.as_deref().and_then(|id| calls.get(id)) else {
            continue;
        };
        let observational = matches!(
            call.name.as_str(),
            "bash_poll" | "read_file" | "grep" | "glob" | "list_directory"
        );
        let key = (
            call.name.as_str(),
            call.arguments.as_str(),
            message.text.as_str(),
            message.is_error,
        );
        if index < start {
            earlier.insert(key);
        } else if (!observational && !message.is_error) || !earlier.contains(&key) {
            progress = true;
        }
    }
    progress
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use atomcode_capabilities::world::{Chunk, Exit, Process, Shell, SpawnError, SpawnOptions};
    use atomcode_kernel::tool::{Tool, ToolContext};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct TestProcess(CancellationToken);
    #[async_trait::async_trait]
    impl Process for TestProcess {
        async fn next_chunk(&self) -> Option<Chunk> {
            self.0.cancelled().await;
            None
        }
        async fn wait(&self) -> Result<Exit, String> {
            self.0.cancelled().await;
            Ok(Exit {
                code: Some(0),
                signal: None,
            })
        }
        async fn kill(&self) {
            self.0.cancel();
        }
    }
    struct TestWorld(Arc<TestProcess>);
    #[async_trait::async_trait]
    impl Shell for TestWorld {
        fn describe(&self) -> String {
            "controlled background job".into()
        }
        async fn spawn(&self, _: &str, _: &SpawnOptions) -> Result<Arc<dyn Process>, SpawnError> {
            Ok(self.0.clone())
        }
    }
    pub(crate) async fn job(notify: bool) -> (ToolCall, ToolResult, CancellationToken) {
        let finish = CancellationToken::new();
        let tool = atomcode_capabilities::tools::bash::BashStartTool::with_world(
            Arc::new(TestWorld(Arc::new(TestProcess(finish.clone())))),
            atomcode_capabilities::tools::sensitive_path::SensitivePaths::new(
                "/tmp/fixture-product",
                ".fixture-product",
            ),
        );
        let call = ToolCall {
            id: "start".into(),
            name: "bash_start".into(),
            arguments: serde_json::json!({"command":"controlled build", "notify_on_exit":notify})
                .to_string(),
        };
        let ctx = ToolContext {
            working_dir: "/tmp".into(),
            cancel: CancellationToken::new(),
            progress: Default::default(),
            requester: None,
        };
        let mut result = tool.execute(&call.arguments, &ctx).await;
        result.call_id = call.id.clone();
        assert!(!result.is_error);
        (call, result, finish)
    }
    async fn collect(completion: &BackgroundCompletion) {
        let ctx = ToolContext {
            working_dir: "/tmp".into(),
            cancel: CancellationToken::new(),
            progress: Default::default(),
            requester: None,
        };
        let result = atomcode_capabilities::tools::bash::BashPollTool
            .execute(
                &serde_json::json!({"job_id":completion.job_id, "wait_ms":0}).to_string(),
                &ctx,
            )
            .await;
        assert!(!result.is_error);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn only_explicit_jobs_notify_and_generation_changes_forget_waits() {
        let (call, result, finish) = job(false).await;
        let mut jobs = BackgroundJobs::default();
        jobs.started(&call);
        jobs.result(&result);
        assert!(!jobs.pending());
        let mut opted = call.clone();
        opted.arguments =
            serde_json::json!({"command":"controlled build", "notify_on_exit":true}).to_string();
        jobs.started(&opted);
        jobs.result(&result);
        assert!(jobs.pending());
        let mut powershell = opted.clone();
        powershell.name = "pwsh_start".into();
        let mut powershell_jobs = BackgroundJobs::default();
        powershell_jobs.started(&powershell);
        powershell_jobs.result(&result);
        assert!(
            powershell_jobs.pending(),
            "PowerShell jobs share completion notifications"
        );
        jobs.retain_generation(1);
        assert!(!jobs.pending());
        jobs.started(&opted);
        jobs.result(&result);
        finish.cancel();
        let completion = jobs.next().await;
        assert_eq!(completion.exit_code, Some(0));
        assert!(!jobs.pending());
        collect(&completion).await;
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn omitted_notification_flag_subscribes_and_collected_results_do_not_notify() {
        let (mut call, result, finish) = job(true).await;
        call.arguments = serde_json::json!({"command":"controlled build"}).to_string();
        let mut jobs = BackgroundJobs::default();
        jobs.started(&call);
        jobs.result(&result);
        assert!(jobs.pending());
        finish.cancel();
        let completion = jobs.next().await;
        assert!(completion
            .notification()
            .unwrap()
            .contains("exit code: Some(0)"));
        collect(&completion).await;
        assert!(completion.notification().is_none());
        assert!(!jobs.uncollected());
    }

    #[test]
    fn a_parked_goal_gets_a_terminal_notice_even_after_the_output_is_collected() {
        let batch = [BackgroundCompletion {
            job_id: "already-collected".into(),
            exit_code: None,
            killed: true,
        }];
        assert!(completion_notice(&batch, false).is_empty());
        let notice = completion_notice(&batch, true);
        assert!(notice.contains("Background job already-collected has finished"));
        assert!(notice.contains("killed: true"));
        assert!(!notice.contains("bash_poll"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn ready_takes_only_completions_that_already_happened() {
        let mut jobs = BackgroundJobs::default();
        let mut finishes = Vec::new();
        for name in ["first", "second", "running"] {
            let (mut call, mut result, finish) = job(true).await;
            call.id = name.into();
            result.call_id = name.into();
            jobs.started(&call);
            jobs.result(&result);
            finishes.push(finish);
        }
        assert!(jobs.ready().is_empty(), "nothing has finished yet");
        finishes[0].cancel();
        finishes[1].cancel();
        let first = jobs.next().await;
        let rest = jobs.ready();
        assert_eq!(rest.len(), 1, "the other finished job joins the batch");
        assert_ne!(rest[0].job_id, first.job_id);
        assert!(jobs.pending(), "the running job is still observed");
        finishes[2].cancel();
        jobs.next().await;
        assert!(!jobs.pending());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_terminal_poll_retires_work_before_the_observer_is_drained() {
        let (call, result, finish) = job(true).await;
        let mut jobs = BackgroundJobs::default();
        jobs.started(&call);
        jobs.result(&result);
        let id = crate::discipline::started_job(&result.content)
            .unwrap()
            .to_owned();
        let observer = observe_completion(&id).unwrap();
        finish.cancel();
        let completion = observer.await;
        collect(&completion).await;
        assert!(jobs.pending(), "the observer still needs draining");
        assert!(
            !jobs.uncollected(),
            "a goal must not wait for collected work"
        );
        assert!(jobs.next().await.notification().is_none());
        assert!(!jobs.pending());
    }

    #[test]
    fn repeated_reads_do_not_count_but_changed_results_and_mutations_do() {
        use atomcode_kernel::message::Message;
        let call = |id: &str, name: &str| {
            Message::assistant(
                "",
                vec![ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: "{}".into(),
                }],
            )
        };
        let mut messages = vec![
            Message::user("initial"),
            call("a", "read_file"),
            Message::tool_result("a", "same", false),
            Message::synthetic_user("continue"),
            call("b", "read_file"),
            Message::tool_result("b", "same", false),
        ];
        assert!(!made_progress(&messages));
        messages.last_mut().unwrap().text = "changed".into();
        assert!(made_progress(&messages));
        messages.extend([
            Message::synthetic_user("continue"),
            call("c", "edit_file"),
            Message::tool_result("c", "same", false),
        ]);
        assert!(made_progress(&messages));
    }
}
