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
//! ## Orphan-safety
//! Honors the same invariant as the foreground tool's Job Object: the reader task OWNS the
//! platform reaper — Unix [`super::PgroupChild`] (`killpg` on `Drop`) / Windows
//! [`crate::process_utils::JobHandle`] (`KILL_ON_JOB_CLOSE` on `Drop`, or when the OS closes
//! the handle). The task lives in the global [`STORE`], so the tree survives across tool
//! calls; a graceful process exit drops the runtime → the task → the reaper → the tree.
//! Only an abrupt SIGKILL of atomcode itself can orphan on Unix (pre-existing and inherent —
//! the foreground tool has the same limit; Windows self-reaps via the OS closing the job
//! handle even then).

use super::{build_command, check_destructive_command, normalize_command_for_grant};
#[cfg(not(target_os = "windows"))]
use super::{sigkill_pgroup, PgroupChild};
use crate::tools::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedReceiver;

/// Per-job output is bounded so a chatty long-runner can't grow the process heap without
/// limit (cf. the grep-OOM lesson). Past this, the OLDEST bytes are dropped and the next
/// poll that would have seen them is flagged truncated — the tail is what a poller needs.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Upper bound on concurrent background jobs. Removal from [`STORE`] happens on the poll
/// that observes a terminal status, so a caller that starts jobs and never polls would grow
/// the map unbounded; this caps it (codex uses the same 64 for its exec sessions) and turns
/// the overflow into a clear, actionable error rather than a silent leak.
const MAX_BACKGROUND_JOBS: usize = 64;

/// After the tracked shell exits, how long to keep draining its pipes before finalizing.
/// A grandchild that inherited stdout (`some-daemon &`) keeps the pipe open after the shell
/// itself exits; without this bound the reader would wait on that pipe forever and the job
/// would report `running` indefinitely. 200ms matches `PgroupChild::terminate`'s grace.
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
    output: Mutex<Buf>,
    status: Mutex<Status>,
}

struct Job {
    command: String,
    shared: Arc<Shared>,
    /// Absolute byte offset already returned by a previous `poll`.
    delivered: usize,
    kill: tokio::sync::mpsc::UnboundedSender<()>,
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
}

/// Spawn `command` detached, register it, and return its job id. Reuses the foreground
/// tool's shell selection, non-interactive env, UTF-8 locale, and tty-detach so a
/// backgrounded command behaves identically to a foreground one — it just isn't awaited.
pub(crate) async fn start(command: &str, ctx: &ToolContext) -> Result<String, String> {
    // Bound the store BEFORE spawning so a runaway starter can't grow it (or leak processes)
    // without limit; count under the lock, drop it before the await-y spawn below.
    if store().lock().unwrap().len() >= MAX_BACKGROUND_JOBS {
        return Err(format!(
            "bash_start: too many background jobs (limit {MAX_BACKGROUND_JOBS}); poll or kill \
             existing ones before starting more."
        ));
    }

    let mut cmd = build_command(command)?;
    cmd.current_dir(&ctx.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    super::apply_non_interactive_env(&mut cmd);
    #[cfg(unix)]
    crate::process_utils::apply_utf8_locale_env(&mut cmd);
    #[cfg(unix)]
    // SAFETY: async-signal-safe libc only — see `detach_child_from_controlling_tty`.
    unsafe {
        cmd.pre_exec(|| {
            super::detach_child_from_controlling_tty();
            Ok(())
        });
    }
    #[cfg(target_os = "windows")]
    crate::process_utils::suppress_console_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("bash_start: failed to spawn shell: {e}"))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let shared = Arc::new(Shared {
        output: Mutex::new(Buf {
            tail: Vec::new(),
            total: 0,
        }),
        status: Mutex::new(Status::Running),
    });
    let (kill_tx, kill_rx) = tokio::sync::mpsc::unbounded_channel();

    #[cfg(not(target_os = "windows"))]
    {
        let reaper = PgroupChild::new(child);
        tokio::spawn(reader_task_unix(
            reaper,
            stdout,
            stderr,
            Arc::clone(&shared),
            kill_rx,
        ));
    }
    #[cfg(target_os = "windows")]
    {
        let job = crate::process_utils::assign_child_to_kill_on_close_job(&child);
        tokio::spawn(reader_task_windows(
            child,
            job,
            stdout,
            stderr,
            Arc::clone(&shared),
            kill_rx,
        ));
    }

    let id = next_id();
    store().lock().unwrap().insert(
        id.clone(),
        Job {
            command: command.to_string(),
            shared,
            delivered: 0,
            kill: kill_tx,
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
    if !matches!(status, Status::Running) {
        // Terminal status is only set AFTER the reader finalizes, so everything is delivered
        // by the `since` above — safe to drop the entry now.
        guard.remove(job_id);
    }
    // Reuse the foreground decode (non-UTF8/GBK/UTF-16) + ANSI/CSI strip so a color- or
    // cursor-emitting long-runner doesn't flood the model with escape bytes.
    let text = super::sanitize_terminal_output(&super::decode_output(&raw));
    Ok(PollResult {
        text,
        truncated,
        status,
    })
}

/// Signal the job's reader to kill the whole process tree. The status flips to `Killed`;
/// the entry is reaped by the next `poll` (which also delivers any final output).
pub(crate) fn kill(job_id: &str) -> Result<String, String> {
    let guard = store().lock().unwrap();
    match guard.get(job_id) {
        Some(job) => {
            let _ = job.kill.send(());
            Ok(job.command.clone())
        }
        None => Err(format!("bash_kill: no background job '{job_id}'")),
    }
}

/// Read one chunk from an optional stream; parks forever when the stream is absent so a
/// disabled `select!` arm (guarded by `*_done`) never resolves spuriously. `Ok(0)` (EOF) and
/// read errors both map to `None` (the stream is done). Cancellation-safe: `AsyncReadExt::read`
/// consumes no bytes when its future is dropped for another `select!` arm.
async fn read_some<R: tokio::io::AsyncRead + Unpin>(s: &mut Option<R>, buf: &mut [u8]) -> Option<usize> {
    match s {
        Some(r) => match r.read(buf).await {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(n),
        },
        None => std::future::pending().await,
    }
}

/// Pump both child streams into `shared` until EOF on both. Platform-agnostic; the reaper is
/// managed separately by the per-platform reader so this loop stays shared.
async fn pump_streams(
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    shared: Arc<Shared>,
) {
    let mut out = stdout;
    let mut errs = stderr;
    let mut out_done = out.is_none();
    let mut err_done = errs.is_none();
    let mut obuf = [0u8; 8192];
    let mut ebuf = [0u8; 8192];
    while !(out_done && err_done) {
        tokio::select! {
            r = read_some(&mut out, &mut obuf), if !out_done => match r {
                None => out_done = true,
                Some(n) => shared.output.lock().unwrap().append(&obuf[..n]),
            },
            r = read_some(&mut errs, &mut ebuf), if !err_done => match r {
                None => err_done = true,
                Some(n) => shared.output.lock().unwrap().append(&ebuf[..n]),
            },
        }
    }
}

/// Map a child's exit result to an exit code, using the `128 + signal` shell convention when
/// it was terminated by a signal (so an OOM-kill reads as `137`, not a misleading `-1`).
fn exit_code_of(status: std::io::Result<std::process::ExitStatus>) -> i32 {
    match status {
        Ok(s) => match s.code() {
            Some(code) => code,
            None => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    return s.signal().map(|sig| 128 + sig).unwrap_or(-1);
                }
                #[cfg(not(unix))]
                {
                    -1
                }
            }
        },
        Err(_) => -1,
    }
}

/// Give the pump a bounded grace to drain buffered output, then finalize the status. The
/// grace bounds the grandchild-holds-pipe hang; a normal fast exit closes its pipes so the
/// pump completes immediately and no time is wasted.
async fn finalize(
    shared: &Arc<Shared>,
    pump: &mut tokio::task::JoinHandle<()>,
    killed: bool,
    code: i32,
) {
    let _ = tokio::time::timeout(POST_EXIT_DRAIN_GRACE, &mut *pump).await;
    pump.abort();
    *shared.status.lock().unwrap() = if killed {
        Status::Killed
    } else {
        Status::Exited(code)
    };
}

#[cfg(not(target_os = "windows"))]
async fn reader_task_unix(
    mut reaper: PgroupChild,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    shared: Arc<Shared>,
    mut kill_rx: UnboundedReceiver<()>,
) {
    let pgid = reaper.pgid();
    let mut pump = tokio::spawn(pump_streams(stdout, stderr, Arc::clone(&shared)));
    let mut killed = false;
    let code = loop {
        tokio::select! {
            biased;
            _ = kill_rx.recv(), if !killed => {
                killed = true;
                sigkill_pgroup(pgid); // borrow-free: doesn't touch `reaper`
            }
            status = reaper.wait_and_disarm() => break exit_code_of(status),
        }
    };
    finalize(&shared, &mut pump, killed, code).await;
}

#[cfg(target_os = "windows")]
async fn reader_task_windows(
    mut child: tokio::process::Child,
    job: Option<crate::process_utils::JobHandle>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    shared: Arc<Shared>,
    mut kill_rx: UnboundedReceiver<()>,
) {
    let pid = child.id();
    let mut pump = tokio::spawn(pump_streams(stdout, stderr, Arc::clone(&shared)));
    let mut killed = false;
    let code = loop {
        tokio::select! {
            biased;
            _ = kill_rx.recv(), if !killed => {
                killed = true;
                crate::process_utils::kill_windows_tree(&job, pid);
            }
            status = child.wait() => break exit_code_of(status),
        }
    };
    finalize(&shared, &mut pump, killed, code).await;
    // `job` drops here → KILL_ON_JOB_CLOSE reaps anything still in the tree.
    drop(job);
}

fn status_line(status: &Status) -> String {
    match status {
        Status::Running => "status: running".to_string(),
        Status::Exited(code) => format!("status: exited (code {code})"),
        Status::Killed => "status: killed".to_string(),
    }
}

#[derive(Deserialize)]
struct StartArgs {
    command: String,
}

#[derive(Deserialize)]
struct JobArgs {
    job_id: String,
}

/// `bash_start` — spawn a command in the background and return its job id.
#[derive(Default)]
pub(crate) struct BashStartTool;

#[async_trait]
impl Tool for BashStartTool {
    fn name(&self) -> &str {
        "bash_start"
    }
    fn description(&self) -> &str {
        "Start a shell command in the BACKGROUND and return a job id immediately (does not \
         wait for it to finish). Use this instead of a large `timeout` for work that legitimately \
         runs longer than the foreground `bash` limit (builds, servers, long test suites). \
         Collect its output later with `bash_poll` (repeat until it reports it exited), and stop \
         it early with `bash_kill`. Same shell, working directory, and environment as `bash`."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to run in the background" }
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
    fn always_grant_scope(&self, args: &str) -> String {
        match serde_json::from_str::<StartArgs>(args) {
            Ok(a) => normalize_command_for_grant(&a.command),
            Err(_) => args.to_string(),
        }
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: StartArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash_start: invalid arguments: {e}. Expected {{\"command\":\"<shell command>\"}}."
                ))
            }
        };
        match start(&a.command, ctx).await {
            Ok(id) => ok(format!(
                "Started background job {id}. Collect output with bash_poll {{\"job_id\":\"{id}\"}} \
                 (repeat until it reports it exited); stop it with bash_kill {{\"job_id\":\"{id}\"}}."
            )),
            Err(e) => err(e),
        }
    }
}

/// `bash_poll` — read new output from a background job and its status.
#[derive(Default)]
pub(crate) struct BashPollTool;

#[async_trait]
impl Tool for BashPollTool {
    fn name(&self) -> &str {
        "bash_poll"
    }
    fn description(&self) -> &str {
        "Read output produced by a background `bash_start` job since your last poll, plus \
         whether it is still running or has exited. Poll repeatedly until it reports it exited. \
         Read-only."
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
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: JobArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "bash_poll: invalid arguments: {e}. Expected {{\"job_id\":\"<id>\"}}."
                ))
            }
        };
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
                body.push_str(&status_line(&r.status));
                ok(body)
            }
            Err(e) => err(e),
        }
    }
}

/// `bash_kill` — stop a background job and reap its whole process tree.
#[derive(Default)]
pub(crate) struct BashKillTool;

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
            Ok(_) => ok(format!(
                "Signalled background job {} to stop. Poll it once more to collect final output.",
                a.job_id
            )),
            Err(e) => err(e),
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    /// Poll at 20ms until terminal or a ~2s budget, accumulating every chunk.
    async fn drain(id: &str) -> (String, Status) {
        let mut collected = String::new();
        let mut status = Status::Running;
        for _ in 0..100 {
            match poll(id) {
                Ok(r) => {
                    collected.push_str(&r.text);
                    status = r.status.clone();
                    if !matches!(status, Status::Running) {
                        break;
                    }
                }
                Err(_) => break, // reaped after a prior terminal poll
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (collected, status)
    }

    #[tokio::test]
    async fn start_survives_the_call_and_poll_collects_output_then_exit() {
        let d = tempfile::tempdir().unwrap();
        let id = start("printf 'hello-bg\\n'", &ctx(d.path())).await.unwrap();
        let (out, status) = drain(&id).await;
        assert!(out.contains("hello-bg"), "output was {out:?}");
        assert_eq!(status, Status::Exited(0));
    }

    #[tokio::test]
    async fn nonzero_exit_code_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let id = start("exit 7", &ctx(d.path())).await.unwrap();
        let (_out, status) = drain(&id).await;
        assert_eq!(status, Status::Exited(7));
    }

    #[tokio::test]
    async fn kill_stops_a_long_running_job() {
        let d = tempfile::tempdir().unwrap();
        let id = start("sleep 30", &ctx(d.path())).await.unwrap();
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
        let id = start("sleep 5 & echo launched", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert!(out.contains("launched"), "output was {out:?}");
        assert_eq!(status, Status::Exited(0), "must not stay Running forever");
    }

    /// Invalid UTF-8 bytes in the middle of output must NOT truncate the stream (the old
    /// line-reader treated the first invalid byte as EOF). Raw-byte reads keep going and the
    /// text after is still delivered (lossily decoded).
    #[tokio::test]
    async fn non_utf8_output_is_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        // 0xFF is invalid UTF-8; "after" follows it.
        let id = start(r"printf '\377bad\nafter\n'", &ctx(d.path()))
            .await
            .unwrap();
        let (out, status) = drain(&id).await;
        assert!(out.contains("after"), "text after invalid byte was lost: {out:?}");
        assert_eq!(status, Status::Exited(0));
    }

    /// ANSI/CSI escapes are stripped by the shared sanitizer, not flooded to the model.
    #[tokio::test]
    async fn ansi_escapes_are_sanitized() {
        let d = tempfile::tempdir().unwrap();
        let id = start(r"printf '\033[31mred\033[0m\n'", &ctx(d.path()))
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
        let id = start("kill -KILL $$", &ctx(d.path())).await.unwrap();
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
