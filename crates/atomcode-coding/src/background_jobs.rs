//! Runtime-owned observations of `bash_start` completion requests.
//! No process ownership: a cancelled turn leaves the job running, so its
//! completion still wakes the model; a generation change forgets notifications
//! and leaves polling available.
use atomcode_capabilities::tools::bash::{observe_completion, BackgroundCompletion};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::tool::{ToolCall, ToolResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Output all background completions delivered in one wakeup share.
const NOTICE_BUDGET_CHARS: usize = 8000;
const NOTICE_TAIL_CHARS: usize = 2000;

pub(crate) fn completion_notice(batch: &[BackgroundCompletion], goal_waiting: bool) -> String {
    let shown: Vec<_> = batch
        .iter()
        .filter(|completion| goal_waiting || !completion.killed)
        .collect();
    // Only output still in the store takes a share: a job `bash_poll` already
    // collected adds no tail, and must not shrink the ones that do.
    let collectable = shown
        .iter()
        .filter(|completion| observe_completion(&completion.job_id).is_some())
        .count();
    let tail = (NOTICE_BUDGET_CHARS / collectable.max(1)).min(NOTICE_TAIL_CHARS);
    shown
        .into_iter()
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

/// The jobs whose completion will start a turn, as the runtime last decided it,
/// shared with the [`BackgroundWaitHint`] that tells a model still polling one.
/// Owned by the parts, so a rebuilt assembly starts from an empty table the way
/// a new generation retires its pending notifications.
#[derive(Clone, Default)]
pub(crate) struct BackgroundWaits(Arc<Mutex<Waits>>);

#[derive(Default, PartialEq)]
struct Waits {
    jobs: HashSet<String>,
    wakes: bool,
}

impl BackgroundWaits {
    fn set(&self, jobs: &HashSet<String>, wakes: bool) {
        let mut waits = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if waits.wakes != wakes || waits.jobs != *jobs {
            *waits = Waits {
                jobs: jobs.clone(),
                wakes,
            };
        }
    }

    fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Waits::default();
    }

    pub(crate) fn wakes_on(&self, job: &str) -> bool {
        let waits = self.0.lock().unwrap_or_else(|e| e.into_inner());
        waits.wakes && waits.jobs.contains(job)
    }
}

/// Reminds a model that polled a job the runtime will wake it for, and found
/// it still running, to end its response instead of polling again: each poll
/// that times out costs a whole request, and the notice is delivered only once
/// the response ends. Mounted only where the driver shows the turn a completion
/// starts. A poll that finds the job finished, a job started without
/// notifications, and a completion that `/loop` or a paused goal would hold
/// back are left alone.
///
/// A request tail, not an edit of the tool result: the result stays the tool's
/// own for the front end that groups quiet polls and the loop guard that tells
/// a background wait from a repeat, and the reminder is never logged.
pub(crate) struct BackgroundWaitHint {
    waits: BackgroundWaits,
}

impl BackgroundWaitHint {
    pub(crate) fn new(waits: BackgroundWaits) -> Self {
        Self { waits }
    }

    fn reminder(&self, messages: &[Message]) -> Option<Message> {
        let jobs: Vec<String> = polled_still_running(messages)
            .into_iter()
            // Still in the job store: a result collected anywhere since (this
            // batch, another session's poll) leaves no notice to wait for.
            .filter(|job| self.waits.wakes_on(job) && observe_completion(job).is_some())
            .collect();
        if jobs.is_empty() {
            return None;
        }
        Some(atomcode_capabilities::reminder::synthetic_system_reminder(
            &format!(
                "Background job {} is still running, and this host resumes you when it \
                 finishes: the completion notice starts your next turn with its result. \
                 Unless you have independent work to do now, end your response instead of \
                 polling again; ending it does not end the task. After the notice, collect \
                 the output once with bash_poll.",
                jobs.join(", ")
            ),
        ))
    }
}

/// The jobs whose every `bash_poll` in the round this request answers reported
/// the job still running. A job one poll of the batch found finished is out:
/// that poll collected its result, and no notice follows a collected job. So
/// is a job the batch stopped with `bash_kill`, whose receipt asks for one more
/// poll and whose completion no notice announces. Nothing once a person has
/// spoken since: then the request answers them.
fn polled_still_running(messages: &[Message]) -> Vec<String> {
    let Some(at) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return Vec::new();
    };
    let answered = &messages[at + 1..];
    if answered
        .iter()
        .any(|m| m.role == Role::User && !m.synthetic)
    {
        return Vec::new();
    }
    let mut jobs: Vec<(String, bool)> = Vec::new();
    for m in answered.iter().filter(|m| m.role == Role::Tool) {
        let Some(call) = messages[at]
            .tool_calls
            .iter()
            .find(|call| Some(&call.id) == m.tool_call_id.as_ref())
            .filter(|call| matches!(call.name.as_str(), "bash_poll" | "bash_kill"))
        else {
            continue;
        };
        let Some(job) = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|args| args.get("job_id")?.as_str().map(str::to_owned))
        else {
            continue;
        };
        // The status line follows the output; a note may follow it.
        let running = call.name == "bash_poll"
            && !m.is_error
            && m.text
                .lines()
                .rev()
                .find(|line| line.starts_with("status: "))
                == Some("status: running");
        match jobs.iter_mut().find(|(seen, _)| *seen == job) {
            Some((_, all_running)) => *all_running &= running,
            None => jobs.push((job, running)),
        }
    }
    jobs.into_iter()
        .filter_map(|(job, all_running)| all_running.then_some(job))
        .collect()
}

#[async_trait::async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for BackgroundWaitHint {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        match self.reminder(&req.messages) {
            Some(reminder) => atomcode_harness::tail::with_tail(req, vec![reminder], next).await,
            None => next.run(req).await,
        }
    }
}

#[derive(Default)]
pub(crate) struct BackgroundJobs {
    generation: u64,
    calls: HashSet<String>,
    pending: HashSet<String>,
    waits: FuturesUnordered<BoxFuture<'static, BackgroundCompletion>>,
    /// Where [`Self::publish`] last wrote the pending jobs.
    published: Option<BackgroundWaits>,
}

impl BackgroundJobs {
    pub fn reset(&mut self, generation: u64) {
        if let Some(published) = self.published.take() {
            published.clear();
        }
        *self = Self {
            generation,
            ..Default::default()
        };
    }
    /// Share which jobs a completion will wake the model for, and whether it
    /// wakes at all right now, with the current assembly's
    /// [`BackgroundWaitHint`]. `None` (no assembly) withdraws the claim.
    pub fn publish(&mut self, waits: Option<&BackgroundWaits>, wakes: bool) {
        let Some(waits) = waits else {
            if let Some(published) = self.published.take() {
                published.clear();
            }
            return;
        };
        if !self
            .published
            .as_ref()
            .is_some_and(|published| Arc::ptr_eq(&published.0, &waits.0))
        {
            if let Some(old) = self.published.replace(waits.clone()) {
                old.clear();
            }
        }
        waits.set(&self.pending, wakes);
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
    /// A job `bash_poll` already collected still completes its observer, but
    /// adds nothing to the notice — nor takes a share of its output budget.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn collected_completions_do_not_shrink_the_notice_tail() {
        let mut jobs = BackgroundJobs::default();
        for name in ["live", "a", "b", "c", "d"] {
            let (mut call, mut result, finish) = job(true).await;
            call.id = name.into();
            result.call_id = name.into();
            jobs.started(&call);
            jobs.result(&result);
            finish.cancel();
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let mut batch = vec![jobs.next().await];
        batch.extend(jobs.ready());
        assert_eq!(batch.len(), 5);
        for completion in &batch[1..] {
            collect(completion).await;
        }
        let text = completion_notice(&batch, false);
        assert!(
            text.contains(&format!("job {} has", batch[0].job_id)),
            "{text}"
        );
        for completion in &batch[1..] {
            assert!(
                !text.contains(&format!("job {} has", completion.job_id)),
                "{text}"
            );
        }
        assert!(
            text.contains(&format!("up to {NOTICE_TAIL_CHARS} characters")),
            "{text}"
        );
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

    /// The request after a round that polled `job` and got `result`.
    fn after_poll(job: &str, result: &str) -> Vec<Message> {
        vec![
            Message::user("build it"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "poll".into(),
                    name: "bash_poll".into(),
                    arguments: serde_json::json!({"job_id":job, "wait_ms":60000}).to_string(),
                }],
            ),
            Message::tool_result("poll", result, false),
        ]
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn only_a_running_job_the_host_will_wake_for_is_reminded() {
        let waits = BackgroundWaits::default();
        let hint = BackgroundWaitHint::new(waits.clone());
        let mut jobs = BackgroundJobs::default();
        let (call, result, finish) = job(true).await;
        let id = crate::discipline::started_job(&result.content)
            .unwrap()
            .to_owned();
        jobs.started(&call);
        jobs.result(&result);
        jobs.publish(Some(&waits), true);
        let running = after_poll(&id, "compiling\nstatus: running");
        let said = hint.reminder(&running).expect("reminded").text;
        assert!(
            said.contains(&id) && said.contains("end your response"),
            "{said}"
        );
        // A note after the status line does not hide the job's state.
        let noted = after_poll(
            &id,
            "status: running\n\n[execution note: wrapped command status]\n...",
        );
        assert!(hint.reminder(&noted).is_some());
        // A poll that found the job finished is the result, not a wait.
        assert!(hint
            .reminder(&after_poll(&id, "done\nstatus: exited (code 0)"))
            .is_none());
        // One batch checks, then waits the job out: the second poll collected
        // its result, so no notice is coming.
        let mut batch = running.clone();
        batch[1].tool_calls.push(ToolCall {
            id: "collect".into(),
            name: "bash_poll".into(),
            arguments: serde_json::json!({"job_id":id, "wait_ms":60000}).to_string(),
        });
        batch.push(Message::tool_result(
            "collect",
            "done\nstatus: exited (code 0)",
            false,
        ));
        assert!(hint.reminder(&batch).is_none());
        // One batch checks, then stops the job: its receipt asks for one more
        // poll, and no notice announces a stopped job.
        let mut stopped = running.clone();
        stopped[1].tool_calls.push(ToolCall {
            id: "stop".into(),
            name: "bash_kill".into(),
            arguments: serde_json::json!({"job_id":id}).to_string(),
        });
        stopped.push(Message::tool_result(
            "stop",
            "Stopped job; collect its output with bash_poll.\nstatus: running",
            false,
        ));
        assert!(hint.reminder(&stopped).is_none());
        // Once the person has spoken, the request answers them.
        let mut spoken = running.clone();
        spoken.push(Message::user("also check the lint"));
        assert!(hint.reminder(&spoken).is_none());

        // `/loop` or a paused goal holds the completion back: no turn starts.
        jobs.publish(Some(&waits), false);
        assert!(hint.reminder(&running).is_none());

        // Cancellation and a new generation retire the notification.
        jobs.publish(Some(&waits), true);
        jobs.reset(1);
        assert!(hint.reminder(&running).is_none());

        // A job started without notifications is waited for by polling.
        let (manual_call, manual, manual_finish) = job(false).await;
        let manual_id = crate::discipline::started_job(&manual.content)
            .unwrap()
            .to_owned();
        jobs.started(&manual_call);
        jobs.result(&manual);
        jobs.publish(Some(&waits), true);
        assert!(hint
            .reminder(&after_poll(&manual_id, "status: running"))
            .is_none());

        // The runtime still lists the job until its notice goes out, but a
        // result collected meanwhile has left nothing to deliver.
        jobs.started(&call);
        jobs.result(&result);
        jobs.publish(Some(&waits), true);
        assert!(hint.reminder(&running).is_some());
        finish.cancel();
        tokio::task::yield_now().await;
        let ctx = ToolContext {
            working_dir: "/tmp".into(),
            cancel: CancellationToken::new(),
            progress: Default::default(),
            requester: None,
        };
        let collected = atomcode_capabilities::tools::bash::BashPollTool
            .execute(
                &serde_json::json!({"job_id":id, "wait_ms":0}).to_string(),
                &ctx,
            )
            .await;
        assert!(collected.content.ends_with("status: exited (code 0)"));
        assert!(waits.wakes_on(&id));
        assert!(hint.reminder(&running).is_none());
        manual_finish.cancel();
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
