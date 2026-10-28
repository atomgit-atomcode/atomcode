//! Runtime-owned observations of explicit `bash_start` completion requests.
//! No process ownership: cancellation forgets notifications and leaves polling available.
use atomcode_capabilities::tools::bash::{observe_completion, BackgroundCompletion};
use atomcode_kernel::tool::{ToolCall, ToolResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet};

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
                    args.get("notify_on_exit").and_then(|v| v.as_bool()) == Some(true)
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
