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
use crate::world::{Exit, Process, Shell, SpawnOptions};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
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
    let process = world.spawn(command, &options).await.map_err(|e| match e {
        crate::world::SpawnError::Unsupported(reason) => reason,
        crate::world::SpawnError::Failed(reason) => format!("bash_start: {reason}"),
    })?;

    let shared = Arc::new(Shared {
        output: Mutex::new(Buf {
            tail: Vec::new(),
            total: 0,
        }),
        status: Mutex::new(Status::Running),
    });
    let (kill_tx, kill_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(reader_task(process, Arc::clone(&shared), kill_rx));

    let id = next_id();
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
    let world = Arc::clone(&job.world);
    if !matches!(status, Status::Running) {
        // Terminal status is only set AFTER the reader finalizes, so everything is delivered
        // by the `since` above — safe to drop the entry now.
        guard.remove(job_id);
    }
    // The world decodes (non-UTF8/GBK/UTF-16 is *its* code page); the ANSI/CSI strip is
    // ours, so a color- or cursor-emitting long-runner doesn't flood the model with escapes.
    let text = super::sanitize_terminal_output(&world.decode(&raw));
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

/// Drain the process's output into `shared` until its pipes close. Arrival order across
/// both pipes is whatever the world hands back — the same interleaving the old two-pipe
/// `select!` produced.
async fn pump(process: Arc<dyn Process>, shared: Arc<Shared>) {
    while let Some(chunk) = process.next_chunk().await {
        shared.output.lock().unwrap().append(chunk.bytes());
    }
}

/// Map how a process ended to an exit code, using the `128 + signal` shell convention when
/// it was terminated by a signal (so an OOM-kill reads as `137`, not a misleading `-1`).
fn exit_code_of(exit: Result<Exit, String>) -> i32 {
    exit.map(|e| e.code_or_signal()).unwrap_or(-1)
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

/// Owns the process for its whole life: pumps output, answers a kill, reaps on exit.
///
/// `kill` and `wait` are both `&self` on the handle, which is what lets one `select!` arm
/// kill while the other is waiting — the borrow the two old per-platform readers had to
/// arrange by copying the pgid / job handle out beforehand.
async fn reader_task(
    process: Arc<dyn Process>,
    shared: Arc<Shared>,
    mut kill_rx: UnboundedReceiver<()>,
) {
    let mut pump = tokio::spawn(pump(Arc::clone(&process), Arc::clone(&shared)));
    let mut killed = false;
    let code = loop {
        tokio::select! {
            biased;
            _ = kill_rx.recv(), if !killed => {
                killed = true;
                process.kill().await;
            }
            exit = process.wait() => break exit_code_of(exit),
        }
    };
    finalize(&shared, &mut pump, killed, code).await;
    // `process` drops here → the local world's reaper (pgroup / KILL_ON_JOB_CLOSE job)
    // takes anything still in the tree with it.
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
pub struct BashStartTool {
    world: Arc<dyn Shell>,
}

impl Default for BashStartTool {
    fn default() -> Self {
        Self {
            world: Arc::new(LocalShell),
        }
    }
}

impl BashStartTool {
    /// Start jobs in `world` instead of on this machine.
    pub fn with_world(world: Arc<dyn Shell>) -> Self {
        Self { world }
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
    /// Same verdict as the foreground `bash` tool, via the SAME function — a backgrounded
    /// command is exactly as dangerous as a foreground one (see `risk` above), so its
    /// "Always" must cover the same ground: session-wide for an ordinary command, pinned to
    /// this command when the arguments name a sensitive path. Keeping a private copy here is
    /// what let the two drift apart in the first place.
    fn always_grant_scope(&self, args: &str) -> String {
        match serde_json::from_str::<StartArgs>(args) {
            Ok(a) => shell_always_grant_scope(args, &a.command),
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
pub struct BashPollTool;

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
