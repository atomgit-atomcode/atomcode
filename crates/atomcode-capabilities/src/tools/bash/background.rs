//! `bash_start` / `bash_poll` / `bash_kill` — a background job path for commands that would
//! exceed the foreground `bash` `timeout` ceiling. A job spawns detached, survives across
//! tool calls, and its output is collected incrementally; the model drives it by polling
//! (no async delivery). This mirrors codex's exec-session model rather than oh-my-pi's
//! auto-delivery, which would need an out-of-band message channel into the agent loop.
//!
//! Command-running entry points (`bash_start`) are gated exactly like foreground `bash` —
//! see [`crate::tools::is_command_shell_tool`], which the workspace / credential / push-label
//! middlewares all key on, so a backgrounded command can't slip past them.
//!
//! ## Where the process actually runs
//! Through the same `shell` seam as the foreground tool ([`crate::world::Shell`]), so a
//! job started from a tree whose world is a container or a read-only sandbox runs *there*.
//! This is the reason the seam is a handle and not a `run()`: a job outlives the tool call
//! that started it, is drained incrementally by later calls, and is killed by a third — the
//! three things a collected result cannot do. Nothing in this file names a shell binary, a
//! pipe, a process group or a job object any more.
//!
//! ## Orphan-safety
//! Honors the same invariant as the foreground tool: the reader task OWNS the
//! [`crate::world::Process`] handle, and the local world's handle owns the platform reaper —
//! Unix `PgroupChild` (`killpg` on `Drop`) / Windows `JobHandle` (`KILL_ON_JOB_CLOSE` on
//! `Drop`, or when the OS closes the handle). The task lives in the global [`STORE`], so the
//! tree survives across tool calls; a graceful process exit drops the runtime → the task →
//! the handle → the reaper → the tree. Only an abrupt SIGKILL of atomcode itself can orphan
//! on Unix (pre-existing and inherent — the foreground tool has the same limit; Windows
//! self-reaps via the OS closing the job handle even then).

use super::{check_destructive_command, shell_always_grant_scope, LocalShell};
use crate::tools::{err, ok};
use crate::world::{Chunk, Exit, Process, Shell, SpawnOptions};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::instrument::WithSubscriber;

/// Per-job output is bounded so a chatty long-runner can't grow the process heap without
/// limit (cf. the grep-OOM lesson). Past this, the OLDEST bytes are dropped and the next
/// poll that would have seen them is flagged truncated — the tail is what a poller needs.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Upper bound on concurrent background jobs. Removal from [`STORE`] happens on the poll
/// that observes a terminal status, so a caller that starts jobs and never polls would grow
/// the map unbounded; this caps it (codex uses the same 64 for its exec sessions) and turns
/// the overflow into a clear, actionable error rather than a silent leak.
const MAX_BACKGROUND_JOBS: usize = 64;

const DEFAULT_POLL_WAIT_MS: u64 = 10_000;
const MAX_POLL_WAIT_MS: u64 = 60_000;

/// After the tracked shell exits, how long its pipes get to close before the job counts as
/// having left something running. A grandchild that inherited stdout (`some-daemon &`)
/// keeps the pipe open after the shell itself exits; past this the job reports the shell's
/// exit, keeps reading what the grandchild writes, and stays killable until it is done.
/// 200ms matches `PgroupChild::terminate`'s grace.
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Status {
    Running,
    /// Clean exit code, OR `128 + signal` when terminated by an EXTERNAL signal (shell
    /// convention) so a signaled job is distinguishable from a program that called `exit(0)`.
    Exited(i32),
    /// Stopped via `bash_kill`.
    Killed,
}

/// Accumulated RAW output bytes as a bounded tail plus the ABSOLUTE count ever appended, so
/// a poll cursor keyed on the absolute offset survives front-drops. Decoding + terminal
/// sanitizing is deferred to `poll` (reusing the foreground pipeline), and raw bytes mean a
/// non-UTF8 (GBK/OEM) chunk is never mistaken for EOF the way a UTF-8 line reader would.
struct Buf {
    tail: Vec<u8>,
    total: usize,
}

impl Buf {
    fn append(&mut self, bytes: &[u8]) {
        self.tail.extend_from_slice(bytes);
        self.total += bytes.len();
        if self.tail.len() > MAX_OUTPUT_BYTES {
            let drop = self.tail.len() - MAX_OUTPUT_BYTES;
            self.tail.drain(0..drop);
        }
    }

    /// Raw bytes appended since absolute offset `delivered`; returns `(bytes, new_delivered,
    /// truncated)` where `truncated` means some undelivered bytes were dropped by the cap.
    fn since(&self, delivered: usize) -> (Vec<u8>, usize, bool) {
        if delivered >= self.total {
            return (Vec::new(), self.total, false);
        }
        let retained_start = self.total - self.tail.len();
        let (bytes, truncated) = if delivered >= retained_start {
            (self.tail[delivered - retained_start..].to_vec(), false)
        } else {
            (self.tail.clone(), true)
        };
        (bytes, self.total, truncated)
    }
}

struct Shared {
    /// Observed lifetime after the world returns a process handle, not CPU time.
    started_at: tokio::time::Instant,
    changed: tokio::sync::Notify,
    finished: std::sync::atomic::AtomicBool,
    output: Mutex<Buf>,
    status: Mutex<Status>,
    /// Diagnostic facts survive incremental polling without retaining unbounded stderr.
    git_auth_failure: std::sync::atomic::AtomicBool,
    /// Do not mistake a folded signal code (128 + signal) for a normal failed exit.
    known_failed_exit: std::sync::atomic::AtomicBool,
    /// The shell has exited but something it started still holds its output open. The
    /// job stays in the store — and `bash_kill` can still reach the tree — until it is gone.
    left_running: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn left_running(&self) -> bool {
        self.left_running.load(Ordering::Acquire)
    }
}

struct Job {
    command: String,
    shared: Arc<Shared>,
    /// Absolute byte offset already returned by a previous `poll`.
    delivered: usize,
    kill: tokio::sync::mpsc::UnboundedSender<()>,
    /// The world the job was started in — `poll` decodes with *its* code page,
    /// not this machine's.
    world: Arc<dyn Shell>,
}

static STORE: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
static COUNTER: AtomicU64 = AtomicU64::new(1);

fn store() -> &'static Mutex<HashMap<String, Job>> {
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_id() -> String {
    format!("bg-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// The result of a `poll`: incremental (decoded + sanitized) text plus the status at read.
pub(crate) struct PollResult {
    pub(crate) text: String,
    pub(crate) truncated: bool,
    pub(crate) status: Status,
    /// The shell exited but what it started is still running (see [`Shared`]).
    pub(crate) left_running: bool,
}

/// Full job termination, after inherited output pipes have closed. Observing
/// does not consume incremental output or remove the job from the poll store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundCompletion {
    pub job_id: String,
    pub exit_code: Option<i32>,
    pub killed: bool,
}

/// Subscribe synchronously, before a poll can collect and remove this job.
/// Dropping the future only stops observation; it never kills the process.
pub fn observe_completion(
    job_id: &str,
) -> Option<futures::future::BoxFuture<'static, BackgroundCompletion>> {
    let shared = store().lock().unwrap().get(job_id)?.shared.clone();
    let job_id = job_id.to_owned();
    Some(Box::pin(async move {
        loop {
            let notified = shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if shared.finished.load(Ordering::Acquire) {
                let status = shared.status.lock().unwrap().clone();
                return BackgroundCompletion {
                    job_id,
                    exit_code: match status {
                        Status::Exited(code) => Some(code),
                        _ => None,
                    },
                    killed: status == Status::Killed,
                };
            }
            notified.await;
        }
    }))
}

/// Spawn `command` in `world`, register it, and return its job id. The world is what
/// makes a backgrounded command behave identically to a foreground one — same shell
/// selection, non-interactive env, UTF-8 locale and tty-detach — because it is the same
/// `spawn`; this function just doesn't await it.
pub(crate) async fn start(
    world: &Arc<dyn Shell>,
    command: &str,
    ctx: &ToolContext,
) -> Result<String, String> {
    // Bound the store BEFORE spawning so a runaway starter can't grow it (or leak processes)
    // without limit; count under the lock, drop it before the await-y spawn below.
    if store().lock().unwrap().len() >= MAX_BACKGROUND_JOBS {
        return Err(format!(
            "bash_start: too many background jobs (limit {MAX_BACKGROUND_JOBS}); poll or kill \
             existing ones before starting more."
        ));
    }

    let options = SpawnOptions {
        cwd: Some(ctx.working_dir.clone()),
        env: Vec::new(),
    };
    let spawn_started = tokio::time::Instant::now();
    let process = world.spawn(command, &options).await.map_err(|e| {
        tracing::info!(
            pid = std::process::id(),
            spawn_ms = spawn_started.elapsed().as_millis() as u64,
            outcome = if matches!(&e, crate::world::SpawnError::Unsupported(_)) {
                "unsupported"
            } else {
                "failed"
            },
            "background_job_start_failed"
        );
        match e {
            crate::world::SpawnError::Unsupported(reason) => reason,
            crate::world::SpawnError::Failed(reason) => format!("bash_start: {reason}"),
        }
    })?;

    let id = next_id();
    let started_at = tokio::time::Instant::now();
    tracing::info!(
        pid = std::process::id(),
        job_id = id.as_str(),
        spawn_ms = (started_at - spawn_started).as_millis() as u64,
        "background_job_started"
    );
    let shared = Arc::new(Shared {
        started_at,
        changed: tokio::sync::Notify::new(),
        finished: std::sync::atomic::AtomicBool::new(false),
        output: Mutex::new(Buf {
            tail: Vec::new(),
            total: 0,
        }),
        status: Mutex::new(Status::Running),
        git_auth_failure: std::sync::atomic::AtomicBool::new(false),
        known_failed_exit: std::sync::atomic::AtomicBool::new(false),
        left_running: std::sync::atomic::AtomicBool::new(false),
    });
    let (kill_tx, kill_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(
        reader_task(
            process,
            Arc::clone(&shared),
            kill_rx,
            Arc::clone(world),
            id.clone(),
        )
        .with_current_subscriber(),
    );
    store().lock().unwrap().insert(
        id.clone(),
        Job {
            command: command.to_string(),
            shared,
            delivered: 0,
            kill: kill_tx,
            world: Arc::clone(world),
        },
    );
    Ok(id)
}

/// Return output appended since the last poll (decoded + terminal-sanitized via the same
/// pipeline as the foreground tool) plus the current status. A FINISHED job is reaped from
/// the store on this call (its output is fully delivered here), so a later poll of the same
/// id reports "no such job".
pub(crate) fn poll(job_id: &str) -> Result<PollResult, String> {
    let mut guard = store().lock().unwrap();
    let job = guard.get_mut(job_id).ok_or_else(|| {
        format!("bash_poll: no background job '{job_id}' (already finished-and-collected, or never started)")
    })?;
    let (raw, new_delivered, truncated) = job.shared.output.lock().unwrap().since(job.delivered);
    job.delivered = new_delivered;
    let status = job.shared.status.lock().unwrap().clone();
    let left_running = job.shared.left_running();
    let world = Arc::clone(&job.world);
    let diagnostic_code = match status {
        Status::Exited(code)
            if !left_running && job.shared.known_failed_exit.load(Ordering::Acquire) =>
        {
            Some(code)
        }
        _ => None,
    };
    let auth_failure = job.shared.git_auth_failure.load(Ordering::Acquire);
    if !matches!(status, Status::Running) && !left_running {
        // Terminal status is only set AFTER the reader finalizes, and `left_running` only
        // clears once the output has ended, so everything is delivered by the `since`
        // above — safe to drop the entry now.
        guard.remove(job_id);
    }
    // The world decodes (non-UTF8/GBK/UTF-16 is *its* code page); the ANSI/CSI strip is
    // ours, so a color- or cursor-emitting long-runner doesn't flood the model with escapes.
    let mut text = super::sanitize_terminal_output(&world.decode(&raw));
    super::append_git_auth_hint(&mut text, diagnostic_code, auth_failure);
    Ok(PollResult {
        text,
        truncated,
        status,
        left_running,
    })
}

/// Wait without consuming output or holding the job store lock. The existing reader
/// remains the only process owner; cancelling this wait leaves the job available.
async fn wait_for_poll(job_id: &str, wait_ms: u64, ctx: &ToolContext) -> Result<(), String> {
    let wait_started = tokio::time::Instant::now();
    let (shared, delivered) = {
        let guard = store().lock().unwrap();
        let job = guard.get(job_id).ok_or_else(|| {
            format!("bash_poll: no background job '{job_id}' (already finished-and-collected, or never started)")
        })?;
        (Arc::clone(&job.shared), job.delivered)
    };
    // Construct only after looking up an internal job id: arbitrary caller input
    // from an invalid id must not be copied into diagnostic logs.
    let mut timing = PollWaitTiming {
        job_id,
        started_at: wait_started,
        requested_wait_ms: wait_ms,
        outcome: "abandoned",
    };
    let initial_status = shared.status.lock().unwrap().clone();
    let initial_left_running = shared.left_running();
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(wait_ms.min(MAX_POLL_WAIT_MS));
    loop {
        // Register BEFORE inspecting state so an output/exit racing this check
        // cannot be missed. Notifications are hints; state is authoritative.
        let changed = shared.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if ctx.cancel.is_cancelled() {
            timing.outcome = "cancelled";
            return Err("bash_poll: wait cancelled; the job was not stopped and its output remains available.".into());
        }
        let has_output = shared.output.lock().unwrap().total > delivered;
        let status = shared.status.lock().unwrap().clone();
        let left_running = shared.left_running();
        if has_output
            || (!matches!(status, Status::Running) && !left_running)
            || status != initial_status
            || left_running != initial_left_running
        {
            timing.outcome = if has_output {
                "output"
            } else if !matches!(status, Status::Running) && !left_running {
                "finished"
            } else {
                "status_changed"
            };
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => {
                timing.outcome = "cancelled";
                return Err("bash_poll: wait cancelled; the job was not stopped and its output remains available.".into());
            }
            _ = tokio::time::sleep_until(deadline) => {
                timing.outcome = "deadline";
                return Ok(());
            }
            _ = &mut changed => {}
        }
    }
}

/// Observability only. Dropping a caller's wait never owns or stops its process.
struct PollWaitTiming<'a> {
    job_id: &'a str,
    started_at: tokio::time::Instant,
    requested_wait_ms: u64,
    outcome: &'static str,
}

impl Drop for PollWaitTiming<'_> {
    fn drop(&mut self) {
        tracing::debug!(
            pid = std::process::id(),
            job_id = self.job_id,
            requested_wait_ms = self.requested_wait_ms,
            effective_wait_ms = self.requested_wait_ms.min(MAX_POLL_WAIT_MS),
            waited_ms = self.started_at.elapsed().as_millis() as u64,
            outcome = self.outcome,
            "background_poll_wait_finished"
        );
    }
}

/// Signal the job's reader to kill the whole process tree — including what the shell left
/// running after it exited. The status flips to `Killed`; the entry is reaped by the next
/// `poll` (which also delivers any final output). Returns what to tell the model: a job
/// that had already finished with nothing left running is said to have finished, not to
/// have been stopped.
pub(crate) fn kill(job_id: &str) -> Result<String, String> {
    let guard = store().lock().unwrap();
    let Some(job) = guard.get(job_id) else {
        return Err(format!("bash_kill: no background job '{job_id}'"));
    };
    // The reader holds the receiving end for as long as anything of the job is alive, so a
    // send that fails means there was nothing left to stop.
    if job.kill.send(()).is_err() {
        let status = job.shared.status.lock().unwrap().clone();
        return Ok(format!(
            "Background job {job_id} (`{}`) had already finished ({}); nothing was running to \
             stop. Poll it once more to collect its output.",
            job.command,
            status_line(&status).trim_start_matches("status: ")
        ));
    }
    Ok(format!(
        "Signalled background job {job_id} (`{}`) to stop. Poll it once more to collect final \
         output.",
        job.command
    ))
}

/// Drain the process's output into `shared` until its pipes close. Arrival order across
/// both pipes is whatever the world hands back — the same interleaving the old two-pipe
/// `select!` produced.
async fn pump(process: Arc<dyn Process>, shared: Arc<Shared>, world: Arc<dyn Shell>) {
    // Retain only a small overlapping window for diagnostics split across chunks.
    // Once matched, retain a boolean, even if subsequent output evicts the message.
    const DIAGNOSTIC_WINDOW: usize = 8 * 1024;
    let mut stderr_tail = Vec::new();
    while let Some(chunk) = process.next_chunk().await {
        if let Chunk::Stderr(bytes) = &chunk {
            if !shared.git_auth_failure.load(Ordering::Acquire) {
                for part in bytes.chunks(DIAGNOSTIC_WINDOW / 2) {
                    stderr_tail.extend_from_slice(part);
                    let decoded = super::sanitize_terminal_output(&world.decode(&stderr_tail));
                    if super::is_git_auth_failure(&decoded) {
                        shared.git_auth_failure.store(true, Ordering::Release);
                        stderr_tail.clear();
                        break;
                    }
                    if stderr_tail.len() > DIAGNOSTIC_WINDOW {
                        stderr_tail.drain(..stderr_tail.len() - DIAGNOSTIC_WINDOW);
                    }
                }
            }
        }
        shared.output.lock().unwrap().append(chunk.bytes());
        shared.changed.notify_waiters();
    }
}

/// Map how a process ended to an exit code, using the `128 + signal` shell convention when
/// it was terminated by a signal (so an OOM-kill reads as `137`, not a misleading `-1`).
fn exit_code_of(exit: Result<Exit, String>) -> i32 {
    exit.map(|e| e.code_or_signal()).unwrap_or(-1)
}

fn set_status(shared: &Shared, killed: bool, code: i32) {
    *shared.status.lock().unwrap() = if killed {
        Status::Killed
    } else {
        Status::Exited(code)
    };
    shared.changed.notify_waiters();
}

/// Owns the process for its whole life: pumps output, answers a kill, reaps on exit.
///
/// `kill` and `wait` are both `&self` on the handle, which is what lets one `select!` arm
/// kill while the other is waiting — the borrow the two old per-platform readers had to
/// arrange by copying the pgid / job handle out beforehand.
async fn reader_task(
    process: Arc<dyn Process>,
    shared: Arc<Shared>,
    mut kill_rx: UnboundedReceiver<()>,
    world: Arc<dyn Shell>,
    job_id: String,
) {
    let mut pump = tokio::spawn(pump(Arc::clone(&process), Arc::clone(&shared), world));
    let mut killed = false;
    let code = loop {
        tokio::select! {
            biased;
            _ = kill_rx.recv(), if !killed => {
                killed = true;
                process.kill().await;
            }
            exit = process.wait() => {
                shared.known_failed_exit.store(
                    exit.as_ref().ok().and_then(|exit| exit.code).is_some_and(|code| code != 0),
                    Ordering::Release,
                );
                break exit_code_of(exit);
            },
        }
    };
    let exited_at = tokio::time::Instant::now();
    tracing::info!(
        pid = std::process::id(),
        job_id = job_id.as_str(),
        observed_runtime_ms = (exited_at - shared.started_at).as_millis() as u64,
        exit_code = code,
        kill_requested = killed,
        "background_job_exited"
    );
    // A normal fast exit closes its pipes, so the pump completes at once and no time is
    // spent here.
    if tokio::time::timeout(POST_EXIT_DRAIN_GRACE, &mut pump)
        .await
        .is_err()
    {
        if killed {
            // A holder that escaped the tree we killed: stop reading for it.
            pump.abort();
        } else {
            // The shell exited and left something running that still writes here — a
            // server it backgrounded. Report the exit, keep collecting what the server
            // says, and keep the tree killable until its output ends. Aborting the pump
            // instead would close the pipe under the server and kill it at its next write.
            shared.left_running.store(true, Ordering::Release);
            set_status(&shared, false, code);
            tokio::select! {
                _ = &mut pump => {}
                Some(()) = kill_rx.recv() => {
                    killed = true;
                    process.kill().await;
                    if tokio::time::timeout(POST_EXIT_DRAIN_GRACE, &mut pump)
                        .await
                        .is_err()
                    {
                        pump.abort();
                    }
                }
            }
        }
    }
    set_status(&shared, killed, code);
    shared.left_running.store(false, Ordering::Release);
    shared.finished.store(true, Ordering::Release);
    shared.changed.notify_waiters();
    let finished_at = tokio::time::Instant::now();
    tracing::info!(
        pid = std::process::id(),
        job_id = job_id.as_str(),
        job_elapsed_ms = (finished_at - shared.started_at).as_millis() as u64,
        post_exit_ms = (finished_at - exited_at).as_millis() as u64,
        kill_requested = killed,
        "background_job_finished"
    );
    // `process` drops here. A shell that was killed takes its tree with it through the
    // local world's reaper (pgroup / job object); one that exited by itself had nothing
    // left holding its output by now.
}

fn status_line(status: &Status) -> String {
    match status {
        Status::Running => "status: running".to_string(),
        Status::Exited(code) => format!("status: exited (code {code})"),
        Status::Killed => "status: killed".to_string(),
    }
}

fn poll_status_line(r: &PollResult) -> String {
    if r.left_running {
        format!(
            "{}; what it started is still running and writing here — keep polling, or stop it \
             with bash_kill",
            status_line(&r.status)
        )
    } else {
        status_line(&r.status)
    }
}

#[derive(Deserialize)]
struct StartArgs {
    command: String,
    #[serde(default)]
    notify_on_exit: bool,
}

#[derive(Deserialize)]
struct JobArgs {
    job_id: String,
}

#[derive(Deserialize)]
struct PollArgs {
    job_id: String,
    #[serde(default)]
    wait_ms: Option<u64>,
}

/// `bash_start` — spawn a command in the background and return its job id.
pub struct BashStartTool {
    world: Arc<dyn Shell>,
    /// What counts as a sensitive target — the same grant floor `bash` uses.
    sensitive: crate::tools::sensitive_path::SensitivePaths,
}

#[cfg(test)]
impl Default for BashStartTool {
    fn default() -> Self {
        Self::new(crate::tools::sensitive_path::test_guard())
    }
}

impl BashStartTool {
    /// Start jobs on this machine.
    pub fn new(sensitive: crate::tools::sensitive_path::SensitivePaths) -> Self {
        Self::with_world(Arc::new(LocalShell), sensitive)
    }

    /// Start jobs in `world` instead of on this machine.
    pub fn with_world(
        world: Arc<dyn Shell>,
        sensitive: crate::tools::sensitive_path::SensitivePaths,
    ) -> Self {
        Self { world, sensitive }
    }
}

#[async_trait]
impl Tool for BashStartTool {
    fn name(&self) -> &str {
        "bash_start"
    }
    fn description(&self) -> &str {
        "Start a shell command in the BACKGROUND and return a job id immediately (does not \
         wait for it to finish). Use this instead of a large `timeout` for work that legitimately \
         runs longer than the foreground `bash` limit (builds, servers, long test suites). \
         Collect its output later with `bash_poll` (waits for output or exit; repeat until it \
         reports it exited), and stop \
         it early with `bash_kill`. Keep build output streaming: `| tail` hides progress and \
         may mask a failed build's exit code. Same shell, working directory, and environment as `bash`."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run in the background" },
                "notify_on_exit": { "type": "boolean", "default": false,
                    "description": "Request a completion notification from a supporting host for bounded work (builds/tests). Leave false for servers/watchers or manual polling. After notification, use bash_poll to collect output." }
            },
            "required": ["command"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        // A backgrounded command is exactly as dangerous as a foreground one — gate the same.
        match serde_json::from_str::<StartArgs>(args) {
            Ok(a) if check_destructive_command(&a.command).is_some() => RiskLevel::Risky,
            Ok(_) => RiskLevel::Safe,
            Err(_) => RiskLevel::Risky,
        }
    }
    /// Same verdict as the foreground `bash` tool, via the SAME function — a backgrounded
    /// command is exactly as dangerous as a foreground one (see `risk` above), so its
    /// "Always" must cover the same ground: session-wide for an ordinary command, pinned to
    /// this command when the arguments name a sensitive path. Keeping a private copy here is
    /// what let the two drift apart in the first place.
    fn always_grant_scope(&self, args: &str) -> String {
        match serde_json::from_str::<StartArgs>(args) {
            Ok(a) => shell_always_grant_scope(&self.sensitive, args, &a.command),
            Err(_) => args.to_string(),
        }
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: StartArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash_start: invalid arguments: {e}. \
                     Expected {{\"command\":\"<shell command>\"}}."
                ))
            }
        };
        match start(&self.world, &a.command, ctx).await {
            Ok(id) if a.notify_on_exit => ok(format!(
                "Started background job {id}. Collect output with bash_poll {{\"job_id\":\"{id}\",\"wait_ms\":60000}} \
                 after the requested host completion notification. Do useful independent work, or finish \
                 this response and wait for the notification; do not repeatedly poll just to wait. \
                 A host without notification support still requires manual polling. \
                 Stop it with bash_kill {{\"job_id\":\"{id}\"}}."
            )),
            Ok(id) => ok(format!(
                "Started background job {id}. Collect output with bash_poll {{\"job_id\":\"{id}\",\"wait_ms\":60000}} \
                 (returns early on output or exit; repeat until it reports it exited). Do not use \
                 sleep or restart the same command while waiting. Stop it with bash_kill {{\"job_id\":\"{id}\"}}."
            )),
            Err(e) => err(e),
        }
    }
}

/// `bash_poll` — read new output from a background job and its status.
#[derive(Default)]
pub struct BashPollTool;

#[async_trait]
impl Tool for BashPollTool {
    fn name(&self) -> &str {
        "bash_poll"
    }
    fn description(&self) -> &str {
        "Read output produced by a background `bash_start` job since your last poll, plus \
         whether it is still running or has exited. Waits up to wait_ms (default 10000, max \
         60000), returning early on output or exit; 0 checks immediately. Use this instead \
         of shell sleep, and repeat until it reports it exited. A wait timeout leaves the \
         job running; it does not mean failure. Do not restart a running build. \
         When a poll has no new output and still reports running, avoid repeated waiting \
         narration or a new status update for each check. Report meaningful new output, \
         completion, failure, or a changed plan instead. Read-only."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job id returned by bash_start" },
                "wait_ms": { "type": "integer", "minimum": 0, "default": 10000, "description": "Maximum wait in milliseconds, clamped to 60000; returns early on output or exit. Use 0 for an immediate check." }
            },
            "required": ["job_id"]
        })
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: PollArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash_poll: invalid arguments: {e}. Expected {{\"job_id\":\"<id>\"}}."
                ))
            }
        };
        if let Err(e) =
            wait_for_poll(&a.job_id, a.wait_ms.unwrap_or(DEFAULT_POLL_WAIT_MS), ctx).await
        {
            return err(e);
        }
        match poll(&a.job_id) {
            Ok(r) => {
                let mut body = String::new();
                if r.truncated {
                    body.push_str("[earlier output dropped — buffer is capped]\n");
                }
                body.push_str(&r.text);
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&poll_status_line(&r));
                ok(body)
            }
            Err(e) => err(e),
        }
    }
}

/// `bash_kill` — stop a background job and reap its whole process tree.
#[derive(Default)]
pub struct BashKillTool;

#[async_trait]
impl Tool for BashKillTool {
    fn name(&self) -> &str {
        "bash_kill"
    }
    fn description(&self) -> &str {
        "Stop a background `bash_start` job and kill its whole process tree. Poll once more \
         afterwards to collect any final output."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string", "description": "The job id returned by bash_start" }
            },
            "required": ["job_id"]
        })
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: JobArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash_kill: invalid arguments: {e}. Expected {{\"job_id\":\"<id>\"}}."
                ))
            }
        };
        match kill(&a.job_id) {
            Ok(message) => ok(message),
            Err(e) => err(e),
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::path::Path;

    /// `bash_start` must reach the SAME "Always allow" verdict as foreground `bash` — its
    /// own `risk` says a backgrounded command is exactly as dangerous. This asserts the two
    /// agree rather than each keeping a private copy of the scope rule, which is exactly how
    /// the foreground bug ("点了总是允许，bash 还是每次都问") survived as long as it did.
    #[test]
    fn always_grant_scope_matches_the_foreground_bash_tool() {
        use crate::tools::bash::BashTool;
        let scope = |cmd: &str| {
            let args = serde_json::json!({ "command": cmd }).to_string();
            (
                BashTool::default().always_grant_scope(&args),
                BashStartTool::default().always_grant_scope(&args),
            )
        };
        // Ordinary command: session-wide on BOTH, so one "Always" stops the next re-prompt.
        let (fg, bg) = scope("rm -rf victim");
        assert_eq!(fg, bg);
        assert_eq!(
            bg, "",
            "a backgrounded ordinary command grants session-wide"
        );
        // Two different ordinary commands share the key.
        assert_eq!(scope("rm -rf a").1, scope("python3 x.py > out.json").1);
        // Sensitive target: command-scoped on BOTH (the hard floor holds in the background too).
        let (fg, bg) = scope("cat ~/.ssh/id_rsa");
        assert_eq!(fg, bg);
        assert_ne!(bg, "", "a sensitive target must not take the tool-wide key");
    }

    fn local() -> Arc<dyn Shell> {
        Arc::new(LocalShell)
    }

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    /// Poll at 20ms until finished (nothing left running) or a ~2s budget, accumulating
    /// every chunk.
    async fn drain(id: &str) -> (String, Status) {
        let mut collected = String::new();
        let mut status = Status::Running;
        for _ in 0..100 {
            match poll(id) {
                Ok(r) => {
                    collected.push_str(&r.text);
                    status = r.status.clone();
                    if !matches!(status, Status::Running) && !r.left_running {
                        break;
                    }
                }
                Err(_) => break, // reaped after a prior terminal poll
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (collected, status)
    }

    /// A world that never spawns anything and says so.
    struct Elsewhere(Mutex<Vec<String>>);

    struct Canned(Mutex<Option<crate::world::Chunk>>);

    #[async_trait]
    impl Process for Canned {
        async fn next_chunk(&self) -> Option<crate::world::Chunk> {
            self.0.lock().unwrap().take()
        }
        async fn wait(&self) -> Result<Exit, String> {
            Ok(Exit {
                code: Some(0),
                signal: None,
            })
        }
        async fn kill(&self) {}
    }

    #[async_trait]
    impl Shell for Elsewhere {
        fn describe(&self) -> String {
            "elsewhere".into()
        }
        async fn spawn(
            &self,
            command: &str,
            _options: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, crate::world::SpawnError> {
            self.0.lock().unwrap().push(command.to_string());
            Ok(Arc::new(Canned(Mutex::new(Some(
                crate::world::Chunk::Stdout(b"ran elsewhere".to_vec()),
            )))))
        }
    }

    #[tokio::test]
    async fn a_job_runs_in_the_world_it_was_started_in() {
        // The claim this file makes since it stopped spawning for itself: point
        // `shell` elsewhere and a background job goes there, and its output comes
        // back from there. `echo` never runs on this machine.
        let d = tempfile::tempdir().unwrap();
        let world = Arc::new(Elsewhere(Mutex::new(Vec::new())));
        let as_shell: Arc<dyn Shell> = world.clone();
        let id = start(&as_shell, "echo local", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert_eq!(out, "ran elsewhere");
        assert_eq!(status, Status::Exited(0));
        assert_eq!(*world.0.lock().unwrap(), vec!["echo local".to_string()]);
    }

    struct AuthProcess {
        chunks: Mutex<std::collections::VecDeque<crate::world::Chunk>>,
        drained: tokio::sync::Notify,
        finish: tokio_util::sync::CancellationToken,
        exit: Exit,
    }

    #[async_trait]
    impl Process for AuthProcess {
        async fn next_chunk(&self) -> Option<crate::world::Chunk> {
            let chunk = self.chunks.lock().unwrap().pop_front();
            if chunk.is_some() {
                return chunk;
            }
            self.drained.notify_one();
            self.finish.cancelled().await;
            None
        }
        async fn wait(&self) -> Result<Exit, String> {
            self.finish.cancelled().await;
            Ok(self.exit)
        }
        async fn kill(&self) {
            self.finish.cancel();
        }
    }

    struct AuthWorld(Arc<AuthProcess>);
    #[async_trait]
    impl Shell for AuthWorld {
        fn describe(&self) -> String {
            "scripted Git output".into()
        }
        async fn spawn(
            &self,
            _: &str,
            _: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, crate::world::SpawnError> {
            Ok(self.0.clone())
        }
    }

    async fn auth_job(chunks: Vec<crate::world::Chunk>, exit: Exit) -> (String, Arc<AuthProcess>) {
        let process = Arc::new(AuthProcess {
            chunks: Mutex::new(chunks.into()),
            drained: tokio::sync::Notify::new(),
            finish: tokio_util::sync::CancellationToken::new(),
            exit,
        });
        let world: Arc<dyn Shell> = Arc::new(AuthWorld(process.clone()));
        let id = start(&world, "git push", &ctx(Path::new(".")))
            .await
            .unwrap();
        process.drained.notified().await;
        (id, process)
    }

    struct PollProcess {
        chunks: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Chunk>>,
        finish: tokio_util::sync::CancellationToken,
    }

    #[async_trait]
    impl Process for PollProcess {
        async fn next_chunk(&self) -> Option<Chunk> {
            let mut chunks = self.chunks.lock().await;
            tokio::select! {
                _ = self.finish.cancelled() => None,
                chunk = chunks.recv() => chunk,
            }
        }
        async fn wait(&self) -> Result<Exit, String> {
            self.finish.cancelled().await;
            Ok(Exit {
                code: Some(0),
                signal: None,
            })
        }
        async fn kill(&self) {
            self.finish.cancel();
        }
    }

    struct PollWorld(Arc<PollProcess>);

    #[async_trait]
    impl Shell for PollWorld {
        fn describe(&self) -> String {
            "scripted build".into()
        }
        async fn spawn(
            &self,
            _: &str,
            _: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, crate::world::SpawnError> {
            Ok(self.0.clone())
        }
    }

    async fn poll_job() -> (
        String,
        tokio::sync::mpsc::UnboundedSender<Chunk>,
        Arc<PollProcess>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let process = Arc::new(PollProcess {
            chunks: tokio::sync::Mutex::new(rx),
            finish: tokio_util::sync::CancellationToken::new(),
        });
        let world: Arc<dyn Shell> = Arc::new(PollWorld(process.clone()));
        let id = start(&world, "build", &ctx(Path::new("."))).await.unwrap();
        (id, tx, process)
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn completion_observation_does_not_consume_output_or_depend_on_poll_store() {
        let (id, process) = auth_job(
            vec![Chunk::Stdout(b"preserved output".to_vec())],
            Exit {
                code: Some(0),
                signal: None,
            },
        )
        .await;
        let observer = observe_completion(&id).expect("registered job");
        process.finish.cancel();
        let (output, status) = drain(&id).await;
        assert_eq!(status, Status::Exited(0));
        // The observer was subscribed before poll removed the job.
        assert_eq!(observer.await.exit_code, Some(0));
        assert_eq!(output, "preserved output");
        assert!(observe_completion(&id).is_none());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn completion_waits_for_lingering_children_and_leaves_output_for_poll() {
        let process = Arc::new(LingeringProcess {
            exit: tokio_util::sync::CancellationToken::new(),
            close_output: tokio_util::sync::CancellationToken::new(),
        });
        let world: Arc<dyn Shell> = Arc::new(TimedPollWorld(process.clone(), Duration::ZERO));
        let id = start(&world, "build", &ctx(Path::new("."))).await.unwrap();
        let mut observer = observe_completion(&id).unwrap();
        process.exit.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut observer)
                .await
                .is_err()
        );
        process.close_output.cancel();
        assert_eq!(observer.await.exit_code, Some(0));
        assert_eq!(drain(&id).await.1, Status::Exited(0));
    }

    #[derive(Clone, Default)]
    struct TimingCapture(Arc<Mutex<Vec<serde_json::Map<String, Value>>>>);

    struct TimingFields<'a>(&'a mut serde_json::Map<String, Value>);

    impl tracing::field::Visit for TimingFields<'_> {
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name().into(), json!(value));
        }
        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            self.0.insert(field.name().into(), json!(value));
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.insert(field.name().into(), json!(value));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().into(), json!(value));
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().into(), json!(format!("{value:?}")));
        }
    }

    impl tracing::Subscriber for TimingCapture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            let mut fields = serde_json::Map::new();
            fields.insert("level".into(), json!(event.metadata().level().as_str()));
            event.record(&mut TimingFields(&mut fields));
            self.0.lock().unwrap().push(fields);
        }
    }

    struct TimedPollWorld(Arc<dyn Process>, Duration);

    #[async_trait]
    impl Shell for TimedPollWorld {
        fn describe(&self) -> String {
            "scripted delayed spawn".into()
        }
        async fn spawn(
            &self,
            _: &str,
            _: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, crate::world::SpawnError> {
            tokio::time::sleep(self.1).await;
            Ok(self.0.clone())
        }
    }

    struct TimedRejectedWorld(bool);

    #[async_trait]
    impl Shell for TimedRejectedWorld {
        fn describe(&self) -> String {
            "scripted rejected spawn".into()
        }
        async fn spawn(
            &self,
            _: &str,
            _: &SpawnOptions,
        ) -> Result<Arc<dyn Process>, crate::world::SpawnError> {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Err(if self.0 {
                crate::world::SpawnError::Unsupported("PRIVATE_SPAWN_ERROR".into())
            } else {
                crate::world::SpawnError::Failed("PRIVATE_SPAWN_ERROR".into())
            })
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rejected_spawn_timing_does_not_log_commands_paths_or_error_text() {
        let capture = TimingCapture::default();
        async {
            for unsupported in [true, false] {
                let world: Arc<dyn Shell> = Arc::new(TimedRejectedWorld(unsupported));
                let error = start(&world, "PRIVATE_COMMAND", &ctx(Path::new("PRIVATE_CWD")))
                    .await
                    .unwrap_err();
                assert_eq!(
                    error,
                    if unsupported {
                        "PRIVATE_SPAWN_ERROR"
                    } else {
                        "bash_start: PRIVATE_SPAWN_ERROR"
                    }
                );
            }
        }
        .with_subscriber(capture.clone())
        .await;
        let events = capture.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        for event in events.iter() {
            assert_eq!(event["message"], "background_job_start_failed");
            assert_eq!(event["level"], "INFO");
            assert_eq!(event["spawn_ms"], 30);
            assert!(!event.contains_key("job_id"));
        }
        assert_eq!(events[0]["outcome"], "unsupported");
        assert_eq!(events[1]["outcome"], "failed");
        assert!(!serde_json::to_string(&*events)
            .unwrap()
            .contains("PRIVATE_"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timing_logs_separate_spawn_runtime_and_poll_wait_without_logging_content() {
        let capture = TimingCapture::default();
        let id = async {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let process = Arc::new(PollProcess {
                chunks: tokio::sync::Mutex::new(rx),
                finish: tokio_util::sync::CancellationToken::new(),
            });
            let world: Arc<dyn Shell> =
                Arc::new(TimedPollWorld(process.clone(), Duration::from_millis(100)));
            let id = start(&world, "PRIVATE_COMMAND", &ctx(Path::new("PRIVATE_CWD")))
                .await
                .unwrap();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                tx.send(Chunk::Stdout(b"PRIVATE_OUTPUT\n".to_vec()))
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(300)).await;
                process.finish.cancel();
            });
            let args = json!({"job_id": id, "wait_ms": 60000}).to_string();
            let first = BashPollTool.execute(&args, &ctx(Path::new("."))).await;
            assert_eq!(first.content, "PRIVATE_OUTPUT\nstatus: running");
            let second = BashPollTool.execute(&args, &ctx(Path::new("."))).await;
            assert_eq!(second.content, "status: exited (code 0)");
            id
        }
        .with_subscriber(capture.clone())
        .await;
        let events = capture.0.lock().unwrap();
        let find = |message: &str| events.iter().find(|e| e["message"] == message).unwrap();
        assert_eq!(find("background_job_started")["spawn_ms"], 100);
        let exit = find("background_job_exited");
        assert_eq!(exit["observed_runtime_ms"], 500);
        assert_eq!(exit["exit_code"], 0);
        assert_eq!(exit["kill_requested"], false);
        let finished = find("background_job_finished");
        assert_eq!(finished["job_elapsed_ms"], 500);
        assert_eq!(finished["post_exit_ms"], 0);
        let polls: Vec<_> = events
            .iter()
            .filter(|e| e["message"] == "background_poll_wait_finished")
            .collect();
        assert_eq!(polls.len(), 2);
        assert_eq!(polls[0]["outcome"], "output");
        assert_eq!(polls[0]["waited_ms"], 200);
        assert_eq!(polls[1]["outcome"], "finished");
        assert_eq!(polls[1]["waited_ms"], 300);
        for event in events.iter() {
            assert_eq!(event["job_id"], id);
            assert_eq!(event["pid"], std::process::id());
        }
        let serialized = serde_json::to_string(&*events).unwrap();
        assert!(
            !serialized.contains("PRIVATE_"),
            "commands, paths and output must stay out of timing logs"
        );
    }

    struct LingeringProcess {
        exit: tokio_util::sync::CancellationToken,
        close_output: tokio_util::sync::CancellationToken,
    }

    #[async_trait]
    impl Process for LingeringProcess {
        async fn next_chunk(&self) -> Option<Chunk> {
            self.close_output.cancelled().await;
            None
        }
        async fn wait(&self) -> Result<Exit, String> {
            self.exit.cancelled().await;
            Ok(Exit {
                code: Some(0),
                signal: None,
            })
        }
        async fn kill(&self) {
            self.exit.cancel();
            self.close_output.cancel();
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timing_logs_do_not_count_lingering_output_as_process_runtime() {
        let capture = TimingCapture::default();
        async {
            let process = Arc::new(LingeringProcess {
                exit: tokio_util::sync::CancellationToken::new(),
                close_output: tokio_util::sync::CancellationToken::new(),
            });
            let world: Arc<dyn Shell> = Arc::new(TimedPollWorld(process.clone(), Duration::ZERO));
            let id = start(&world, "build", &ctx(Path::new("."))).await.unwrap();
            let started = tokio::time::Instant::now();
            tokio::spawn(async move {
                tokio::time::sleep_until(started + Duration::from_millis(100)).await;
                process.exit.cancel();
                tokio::time::sleep_until(started + Duration::from_millis(700)).await;
                process.close_output.cancel();
            });
            assert_eq!(drain(&id).await.1, Status::Exited(0));
        }
        .with_subscriber(capture.clone())
        .await;
        let events = capture.0.lock().unwrap();
        let exit = events
            .iter()
            .find(|e| e["message"] == "background_job_exited")
            .unwrap();
        let finished = events
            .iter()
            .find(|e| e["message"] == "background_job_finished")
            .unwrap();
        assert_eq!(exit["observed_runtime_ms"], 100);
        assert_eq!(finished["job_elapsed_ms"], 700);
        assert_eq!(finished["post_exit_ms"], 600);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timing_logs_cover_timeout_cancel_and_dropped_wait_without_stopping_the_job() {
        let capture = TimingCapture::default();
        async {
            let (id, _tx, process) = poll_job().await;
            let context = ctx(Path::new("."));
            wait_for_poll(&id, 20, &context).await.unwrap();
            let cancel = context.cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                cancel.cancel();
            });
            assert!(wait_for_poll(&id, 1000, &context).await.is_err());
            let fresh = ctx(Path::new("."));
            let mut pending = Box::pin(wait_for_poll(&id, 10000, &fresh));
            tokio::select! {
                _ = &mut pending => panic!("the silent job should still be running"),
                _ = tokio::time::sleep(Duration::from_millis(75)) => {}
            }
            drop(pending);
            assert!(
                !process.finish.is_cancelled(),
                "timing observers must not stop the job"
            );
            assert!(wait_for_poll("PRIVATE_INVALID_JOB", 1000, &fresh)
                .await
                .is_err());
            kill(&id).unwrap();
            assert_eq!(drain(&id).await.1, Status::Killed);
        }
        .with_subscriber(capture.clone())
        .await;
        let events = capture.0.lock().unwrap();
        let polls: Vec<_> = events
            .iter()
            .filter(|e| e["message"] == "background_poll_wait_finished")
            .collect();
        assert_eq!(polls.len(), 3);
        for (event, reason, elapsed) in [
            (polls[0], "deadline", 20),
            (polls[1], "cancelled", 100),
            (polls[2], "abandoned", 75),
        ] {
            assert_eq!(event["level"], "DEBUG");
            assert_eq!(event["outcome"], reason);
            assert_eq!(event["waited_ms"], elapsed);
        }
        assert!(!serde_json::to_string(&*events)
            .unwrap()
            .contains("PRIVATE_INVALID_JOB"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_waiting_poll_returns_early_on_output_then_on_exit() {
        let (id, tx, process) = poll_job().await;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            tx.send(Chunk::Stdout(b"compiled module\n".to_vec()))
                .unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            process.finish.cancel();
        });
        let args = json!({"job_id": id, "wait_ms": 60000}).to_string();
        let started = tokio::time::Instant::now();
        let first = BashPollTool.execute(&args, &ctx(Path::new("."))).await;
        assert!(!first.is_error, "{}", first.content);
        assert!(first.content.contains("compiled module"));
        assert!(first.content.ends_with("status: running"));
        assert_eq!(started.elapsed(), Duration::from_millis(200));
        let second = BashPollTool.execute(&args, &ctx(Path::new("."))).await;
        assert!(!second.is_error, "{}", second.content);
        assert!(
            !second.content.contains("compiled module"),
            "output is incremental"
        );
        assert!(second.content.ends_with("status: exited (code 0)"));
        assert_eq!(started.elapsed(), Duration::from_millis(500));
        assert!(poll(&id).is_err(), "a collected terminal job is reaped");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn poll_wait_is_bounded_and_does_not_stop_a_silent_job() {
        let (id, _tx, process) = poll_job().await;
        for (args, expected_ms) in [
            (json!({"job_id": id}), 10000),
            (json!({"job_id": id, "wait_ms": null}), 10000),
            (json!({"job_id": id, "wait_ms": 0}), 0),
            (json!({"job_id": id, "wait_ms": u64::MAX}), 60000),
        ] {
            let started = tokio::time::Instant::now();
            let result = BashPollTool
                .execute(&args.to_string(), &ctx(Path::new(".")))
                .await;
            assert!(!result.is_error, "{}", result.content);
            assert_eq!(result.content, "status: running");
            assert_eq!(started.elapsed(), Duration::from_millis(expected_ms));
            assert!(
                !process.finish.is_cancelled(),
                "a poll timeout must not kill the build"
            );
        }
        kill(&id).unwrap();
        assert_eq!(drain(&id).await.1, Status::Killed);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cancelling_a_poll_preserves_the_job_and_unread_output() {
        let (id, tx, process) = poll_job().await;
        let context = ctx(Path::new("."));
        let cancel = context.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.cancel();
        });
        let args = json!({"job_id": id}).to_string();
        let started = tokio::time::Instant::now();
        let result = BashPollTool.execute(&args, &context).await;
        assert!(result.is_error);
        assert!(result.content.contains("wait cancelled"));
        assert_eq!(started.elapsed(), Duration::from_millis(100));
        assert!(!process.finish.is_cancelled());
        tx.send(Chunk::Stdout(b"still building\n".to_vec()))
            .unwrap();
        // Even a pre-cancelled poll must not consume this chunk.
        assert!(BashPollTool.execute(&args, &context).await.is_error);
        let output = BashPollTool.execute(&args, &ctx(Path::new("."))).await;
        assert!(output.content.contains("still building"));
        kill(&id).unwrap();
        assert_eq!(drain(&id).await.1, Status::Killed);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_missing_job_poll_fails_without_waiting() {
        let started = tokio::time::Instant::now();
        let result = BashPollTool
            .execute(
                r#"{"job_id":"bg-never-started","wait_ms":60000}"#,
                &ctx(Path::new(".")),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("no background job"));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn git_auth_hint_survives_split_stderr_and_earlier_truncated_polls() {
        use crate::world::Chunk;
        let (id, process) = auth_job(
            vec![
                Chunk::Stderr([vec![b'z'; 32 * 1024], vec![b'\n']].concat()),
                Chunk::Stderr(b"fatal: could not read User".to_vec()),
                Chunk::Stderr(
                    b"name for 'https://example.com': terminal prompts disabled\n".to_vec(),
                ),
                Chunk::Stdout(vec![b'x'; MAX_OUTPUT_BYTES + 100]),
            ],
            Exit {
                code: Some(128),
                signal: None,
            },
        )
        .await;
        let early = poll(&id).unwrap();
        assert!(early.truncated);
        assert_eq!(early.status, Status::Running);
        assert!(!early.text.contains("[hint]"));
        process.finish.cancel();
        let (text, status) = drain(&id).await;
        assert_eq!(status, Status::Exited(128));
        assert_eq!(text.matches("[hint]").count(), 1, "{text}");
        assert!(text.contains("Git credential"), "{text}");
        assert!(poll(&id).is_err(), "a finished job is reaped");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn background_git_auth_hints_require_stderr_and_a_known_failed_exit() {
        use crate::world::Chunk;
        let diagnostic = b"fatal: Authentication failed for 'https://example.com/repo.git'\n";
        for (chunk, exit, cancelled) in [
            (
                Chunk::Stdout(diagnostic.to_vec()),
                Exit {
                    code: Some(128),
                    signal: None,
                },
                false,
            ),
            (
                Chunk::Stderr(b"Authentication failed\n".to_vec()),
                Exit {
                    code: Some(1),
                    signal: None,
                },
                false,
            ),
            (
                Chunk::Stderr(diagnostic.to_vec()),
                Exit {
                    code: Some(0),
                    signal: None,
                },
                false,
            ),
            (
                Chunk::Stderr(diagnostic.to_vec()),
                Exit {
                    code: None,
                    signal: Some(9),
                },
                false,
            ),
            (
                Chunk::Stderr(diagnostic.to_vec()),
                Exit {
                    code: None,
                    signal: None,
                },
                false,
            ),
            (
                Chunk::Stderr(diagnostic.to_vec()),
                Exit {
                    code: Some(128),
                    signal: None,
                },
                true,
            ),
        ] {
            let (id, process) = auth_job(vec![chunk], exit).await;
            let early = poll(&id).unwrap();
            assert!(!early.text.contains("[hint]"));
            if cancelled {
                kill(&id).unwrap();
            } else {
                process.finish.cancel();
            }
            let (text, status) = drain(&id).await;
            assert!(!text.contains("[hint]"), "{status:?}: {text}");
            assert_eq!(
                status,
                if cancelled {
                    Status::Killed
                } else {
                    Status::Exited(exit.code_or_signal())
                }
            );
        }
    }

    #[tokio::test]
    async fn start_survives_the_call_and_poll_collects_output_then_exit() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), "printf 'hello-bg\\n'", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert!(out.contains("hello-bg"), "output was {out:?}");
        assert_eq!(status, Status::Exited(0));
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), "exit 7", &ctx(d.path())).await.unwrap();
        let (_out, status) = drain(&id).await;
        assert_eq!(status, Status::Exited(7));
    }

    #[tokio::test]
    async fn kill_stops_a_long_running_job() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), "sleep 30", &ctx(d.path())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await; // let the shell come up
        kill(&id).unwrap();
        let (_out, status) = drain(&id).await;
        assert_eq!(status, Status::Killed);
    }

    #[tokio::test]
    async fn poll_unknown_job_is_an_error() {
        assert!(poll("bg-does-not-exist").is_err());
    }

    /// A grandchild that inherits stdout (`daemon &`) must NOT hang the job forever: the
    /// shell exits, the grace elapses, and the status finalizes to Exited.
    #[tokio::test]
    async fn grandchild_holding_pipe_does_not_hang_the_job() {
        let d = tempfile::tempdir().unwrap();
        // `sleep 5` inherits stdout and outlives the shell, which exits right after echo.
        let id = start(&local(), "sleep 5 & echo launched", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert!(out.contains("launched"), "output was {out:?}");
        assert_eq!(status, Status::Exited(0), "must not stay Running forever");
    }

    /// What the shell left running survives it, keeps reporting through the job, and the
    /// job stays until it is done — rather than the pipe closing under it and its next
    /// write killing it.
    #[tokio::test]
    async fn what_a_job_left_running_keeps_writing_through_it() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("survived");
        let command = format!(
            "(sleep 0.6; echo later && touch '{}') & echo launched",
            marker.display()
        );
        let id = start(&local(), &command, &ctx(d.path())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let first = poll(&id).unwrap();
        assert!(first.text.contains("launched"), "{:?}", first.text);
        assert_eq!(first.status, Status::Exited(0));
        assert!(
            first.left_running,
            "the shell's child still holds the output"
        );
        assert!(poll_status_line(&first).contains("bash_kill"));
        let mut later = String::new();
        for _ in 0..100 {
            match poll(&id) {
                Ok(r) => {
                    later.push_str(&r.text);
                    if !r.left_running {
                        break;
                    }
                }
                Err(_) => break,
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(later.contains("later"), "{later:?}");
        assert!(marker.exists(), "the child died writing to its output");
        assert!(poll(&id).is_err(), "reaped once nothing is left running");
    }

    /// `bash_kill` still reaches what the shell left running after it exited.
    #[tokio::test]
    async fn kill_reaches_what_an_exited_job_left_running() {
        let d = tempfile::tempdir().unwrap();
        let pidfile = d.path().join("pid");
        let command = format!(
            "sleep 30 & echo $! > '{}'; echo launched",
            pidfile.display()
        );
        let id = start(&local(), &command, &ctx(d.path())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let r = poll(&id).unwrap();
        assert!(r.left_running);
        let message = kill(&id).unwrap();
        assert!(message.contains("Signalled"), "{message}");
        let (_out, status) = drain(&id).await;
        assert_eq!(status, Status::Killed);
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // Reparented to init once its shell went, so give init a moment to reap it.
        let mut alive = true;
        for _ in 0..40 {
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!alive, "the backgrounded sleep {pid} outlived bash_kill");
    }

    /// A job that finished with nothing left running is not "stopped" by `bash_kill`.
    #[tokio::test]
    async fn kill_after_a_job_finished_says_so() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), "echo hi", &ctx(d.path())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let message = kill(&id).unwrap();
        assert!(message.contains("already finished"), "{message}");
        assert!(message.contains("exited (code 0)"), "{message}");
        let r = poll(&id).unwrap();
        assert!(r.text.contains("hi"));
    }

    /// Invalid UTF-8 bytes in the middle of output must NOT truncate the stream (the old
    /// line-reader treated the first invalid byte as EOF). Raw-byte reads keep going and the
    /// text after is still delivered (lossily decoded).
    #[tokio::test]
    async fn non_utf8_output_is_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        // 0xFF is invalid UTF-8; "after" follows it.
        let id = start(&local(), r"printf '\377bad\nafter\n'", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert!(
            out.contains("after"),
            "text after invalid byte was lost: {out:?}"
        );
        assert_eq!(status, Status::Exited(0));
    }

    /// ANSI/CSI escapes are stripped by the shared sanitizer, not flooded to the model.
    #[tokio::test]
    async fn ansi_escapes_are_sanitized() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), r"printf '\033[31mred\033[0m\n'", &ctx(d.path()))
            .await
            .unwrap();
        let (out, _status) = drain(&id).await;
        assert!(out.contains("red"), "output was {out:?}");
        assert!(!out.contains('\u{1b}'), "escape bytes leaked: {out:?}");
    }

    /// A shell terminated by an external signal reports `128 + signal` (137 for SIGKILL),
    /// not a misleading `-1` and not `Killed` (which is reserved for our own bash_kill).
    #[tokio::test]
    async fn external_signal_reports_128_plus_signal() {
        let d = tempfile::tempdir().unwrap();
        let id = start(&local(), "kill -KILL $$", &ctx(d.path()))
            .await
            .unwrap();
        let (_out, status) = drain(&id).await;
        assert_eq!(status, Status::Exited(137));
    }

    #[test]
    fn buf_since_delivers_increments_and_flags_truncation() {
        let mut b = Buf {
            tail: Vec::new(),
            total: 0,
        };
        b.append(b"one\n");
        let (bytes, delivered, truncated) = b.since(0);
        assert_eq!(bytes, b"one\n");
        assert_eq!(delivered, 4);
        assert!(!truncated);
        // Nothing new since the cursor.
        let (bytes, _, _) = b.since(delivered);
        assert!(bytes.is_empty());
    }
}
