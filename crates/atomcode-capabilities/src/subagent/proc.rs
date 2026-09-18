//! Managed child process for external-agent drivers.
//!
//! A thin wrapper over `tokio::process::Child` that reaps the WHOLE process tree
//! on cancel/timeout — the external agents (`codex`, `claude`) themselves spawn
//! grandchildren (language servers, tool subprocesses) that a direct-child kill
//! would orphan. This mirrors the reaping the bash tool already does:
//!
//! - Windows: a kill-on-close Job Object (`process_utils`); dropping the guard
//!   or an explicit `kill_tree()` reaps the tree, with a `taskkill /T` fallback.
//! - Unix: `setsid()` in `pre_exec` makes the child its own pgroup leader so
//!   `killpg(pid)` reaches detached grandchildren; `kill_on_drop` covers the
//!   direct child.
//!
//! Phase 1 / T1.2 of the external-agent subagent-driver spec. This is the shared
//! subprocess primitive; the Codex / Claude Code adapters (T1.3 / T1.4) build on
//! it.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

use super::SubagentError;

/// Keep at most this many bytes of stderr for a failure message.
pub const STDERR_TAIL_CAP: usize = 2000;

/// Grace to reap the child AFTER both its pipes have closed. Once stdout+stderr
/// hit EOF the process is exiting; this bounds the wait for the exit status
/// WITHOUT re-arming the full run timeout (which would let a wedged post-close
/// child stall for up to ~2× the configured ceiling). Generous enough for a
/// normal teardown (flush the `-o` file, reap the agent's own subprocesses).
const POST_DRAIN_GRACE: Duration = Duration::from_secs(30);

/// Whether the child gets a writable stdin pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    /// stdin is `/dev/null` (the agent takes its prompt via args).
    Null,
    /// stdin is a pipe (for a JSON-RPC / streaming protocol).
    Piped,
}

/// How to spawn a managed child.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Program to run (resolved binary name or path; looked up on `PATH`).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: PathBuf,
    /// Extra environment entries (merged onto the inherited environment).
    pub env: Vec<(String, String)>,
    /// stdin disposition.
    pub stdin: StdinMode,
}

impl ChildSpec {
    /// A stdin-null spec (prompt-via-args, the common case for `codex exec` and
    /// `claude -p`).
    pub fn new(program: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            stdin: StdinMode::Null,
        }
    }

    /// Set the argument vector.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }
}

/// Result of racing a child's exit against a timeout and a cancel token.
#[derive(Debug)]
pub enum WaitOutcome {
    /// The child exited on its own; the tree was NOT force-killed.
    Exited(ExitStatus),
    /// The overall timeout elapsed; the tree was killed.
    TimedOut,
    /// The cancel token fired; the tree was killed.
    Cancelled,
}

/// A spawned child whose whole process tree is reaped on drop / cancel / timeout.
pub struct ManagedChild {
    child: Child,
    /// PID captured at spawn: the pgroup leader on Unix, the `taskkill /T`
    /// fallback root on Windows.
    pid: Option<u32>,
    /// The child was already reaped by a completed `wait()`. Once true, the pid
    /// is freed and the OS may recycle it, so `Drop` must NOT `killpg` it (that
    /// could SIGKILL an unrelated recycled process group).
    reaped: bool,
    #[cfg(windows)]
    job: Option<crate::process_utils::JobHandle>,
}

impl ManagedChild {
    /// Spawn the child with piped stdout/stderr and the requested stdin mode.
    /// The tree-reaping machinery is installed before returning.
    pub fn spawn(spec: ChildSpec) -> Result<Self, SubagentError> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd.stdin(match spec.stdin {
            StdinMode::Null => Stdio::null(),
            StdinMode::Piped => Stdio::piped(),
        });
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        // No console-window flash per spawn on Windows (headless/daemon). No-op elsewhere.
        crate::process_utils::suppress_console_window(&mut cmd);

        // Unix: setsid in pre_exec so the child leads its own pgroup — killpg then
        // reaches grandchildren the direct-child kill_on_drop would orphan. Mirror
        // the bash tool exactly (async-signal-safe: setsid only, no alloc/locks).
        #[cfg(unix)]
        unsafe {
            // tokio::process::Command::pre_exec (native, no CommandExt needed).
            cmd.pre_exec(|| {
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }

        let child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SubagentError::BinaryNotFound {
                    binary: spec.program.display().to_string(),
                }
            } else {
                SubagentError::SpawnFailed(e.to_string())
            }
        })?;

        #[cfg(windows)]
        let job = crate::process_utils::assign_child_to_kill_on_close_job(&child);
        let pid = child.id();

        Ok(Self {
            child,
            pid,
            reaped: false,
            #[cfg(windows)]
            job,
        })
    }

    /// Take the child's stdout for streaming (once).
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the child's stderr for streaming / diagnostics (once).
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Take the child's stdin (only present when spawned with [`StdinMode::Piped`]).
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Force-kill the whole process tree (best effort).
    pub fn kill_tree(&self) {
        #[cfg(windows)]
        crate::process_utils::kill_windows_tree(&self.job, self.pid);
        #[cfg(not(windows))]
        if let Some(pid) = self.pid {
            // SIGKILL the pgroup; kill_on_drop already covers the direct child,
            // this extends it to detached grandchildren. ESRCH (empty group) is
            // harmless and ignored.
            unsafe { killpg(pid as i32, SIGKILL) };
        }
    }

    /// Wait for the child to exit (no timeout, no cancel).
    pub async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() {
            self.reaped = true;
        }
        status
    }

    /// Write `data` to the child's stdin and close it (EOF), off-task so a large
    /// prompt can't deadlock against the caller draining stdout. No-op if stdin
    /// was not piped ([`StdinMode::Piped`]).
    pub fn write_stdin_and_close(&mut self, data: String) {
        if let Some(mut stdin) = self.child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(data.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }
    }

    /// Race the child's exit against `timeout` and `cancel`. On either, the whole
    /// tree is killed and the corresponding outcome is returned; otherwise the
    /// natural exit status is returned. `cancel` is checked with `biased`
    /// priority so a pending cancel wins over a just-elapsed timeout.
    pub async fn wait_or_kill(
        &mut self,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> std::io::Result<WaitOutcome> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.kill_tree();
                Ok(WaitOutcome::Cancelled)
            }
            res = tokio::time::timeout(timeout, self.child.wait()) => match res {
                Ok(Ok(status)) => {
                    // Child reaped: mark it so Drop won't killpg a possibly-recycled pid.
                    self.reaped = true;
                    Ok(WaitOutcome::Exited(status))
                }
                Ok(Err(e)) => Err(e),
                Err(_elapsed) => {
                    self.kill_tree();
                    Ok(WaitOutcome::TimedOut)
                }
            }
        }
    }
}

/// Drain BOTH of a child's pipes to completion (so a chatty stderr can't fill
/// its buffer and wedge the child) while racing cancel + timeout, then resolve
/// the exit status. Each stdout line is handed to `on_stdout_line`; stderr is
/// accumulated into a capped tail returned alongside the outcome.
///
/// Shared by the Codex and Claude Code adapters. On cancel/timeout the tree is
/// killed and the corresponding outcome is returned immediately (the caller
/// decides what partial output to surface). `stdout`/`stderr` are taken from the
/// child; call before `wait`.
pub async fn drain_and_wait<F: FnMut(String)>(
    child: &mut ManagedChild,
    timeout: Duration,
    cancel: &CancellationToken,
    mut on_stdout_line: F,
) -> std::io::Result<(WaitOutcome, String)> {
    let mut out_lines = child.take_stdout().map(|s| BufReader::new(s).lines());
    let mut err_lines = child.take_stderr().map(|s| BufReader::new(s).lines());
    let mut stderr_tail = String::new();
    let mut stdout_done = out_lines.is_none();
    let mut stderr_done = err_lines.is_none();

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    while !(stdout_done && stderr_done) {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                child.kill_tree();
                return Ok((WaitOutcome::Cancelled, stderr_tail));
            }
            _ = &mut deadline => {
                child.kill_tree();
                return Ok((WaitOutcome::TimedOut, stderr_tail));
            }
            line = next_line(&mut out_lines), if !stdout_done => match line {
                Some(l) => on_stdout_line(l),
                None => stdout_done = true,
            },
            line = next_line(&mut err_lines), if !stderr_done => match line {
                Some(l) => push_tail(&mut stderr_tail, &l),
                None => stderr_done = true,
            },
        }
    }
    // Pipes closed → the process is exiting; reap with a short grace (not the
    // full timeout again) while still honoring cancel.
    let outcome = child.wait_or_kill(POST_DRAIN_GRACE, cancel).await?;
    Ok((outcome, stderr_tail))
}

/// Await the next line of an optional reader; parks forever when `None` (the
/// caller guards the select branch so a parked future is never polled).
async fn next_line<R: AsyncBufRead + Unpin>(lines: &mut Option<Lines<R>>) -> Option<String> {
    match lines {
        Some(l) => l.next_line().await.ok().flatten(),
        None => std::future::pending().await,
    }
}

/// Append a line to a capped stderr tail (keeping the LAST [`STDERR_TAIL_CAP`]
/// bytes, cut on a char boundary).
pub fn push_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    if tail.len() > STDERR_TAIL_CAP {
        let mut idx = tail.len() - STDERR_TAIL_CAP;
        while idx < tail.len() && !tail.is_char_boundary(idx) {
            idx += 1;
        }
        *tail = tail.split_off(idx);
    }
}

/// RAII backstop: if the `ManagedChild` is dropped WITHOUT going through
/// cancel/timeout (e.g. the kernel drops the tool's execute future during a
/// shutdown/abort rather than firing the cancel token), reap the WHOLE tree.
/// `kill_on_drop` alone only reaps the direct child, orphaning the setsid-pgroup
/// grandchildren the driven agent spawned — exactly what this exists to prevent.
/// Best-effort and idempotent (killpg ESRCH / a closed job handle are ignored).
impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Only reap the tree if the child was NOT already reaped by a completed
        // wait(): after reaping, the pid is freed and the OS may recycle its pgid,
        // so a `killpg` here could SIGKILL an unrelated group. On the un-reaped
        // drop path (future dropped without cancel/timeout), the pid is still ours
        // and killpg correctly reaps the setsid grandchildren `kill_on_drop` alone
        // would orphan. Best-effort and idempotent.
        if !self.reaped {
            self.kill_tree();
        }
    }
}

#[cfg(not(target_os = "windows"))]
extern "C" {
    fn killpg(pgid: i32, sig: i32) -> i32;
}
#[cfg(not(target_os = "windows"))]
const SIGKILL: i32 = 9;

#[cfg(test)]
mod unit_tests {
    use super::push_tail;
    use super::STDERR_TAIL_CAP;

    #[test]
    fn push_tail_keeps_last_bytes_on_char_boundary() {
        let mut tail = String::new();
        for i in 0..1000 {
            push_tail(&mut tail, &format!("line-{i}-日本語"));
        }
        assert!(tail.len() <= STDERR_TAIL_CAP + 64);
        assert!(std::str::from_utf8(tail.as_bytes()).is_ok());
        assert!(tail.contains("line-999"), "most recent line survives");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn sh(script: &str) -> ChildSpec {
        ChildSpec::new("/bin/sh", std::env::temp_dir()).args(["-c", script])
    }

    #[tokio::test]
    async fn spawns_captures_stdout_and_exits_zero() {
        let mut child = ManagedChild::spawn(sh("echo hello-subagent")).unwrap();
        let stdout = child.take_stdout().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let first = lines.next_line().await.unwrap();
        assert_eq!(first.as_deref(), Some("hello-subagent"));
        let outcome = child
            .wait_or_kill(Duration::from_secs(5), &CancellationToken::new())
            .await
            .unwrap();
        match outcome {
            WaitOutcome::Exited(s) => assert!(s.success()),
            other => panic!("expected clean exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_maps_to_binary_not_found() {
        let spec = ChildSpec::new("/nonexistent/atomcode-no-such-bin", std::env::temp_dir());
        match ManagedChild::spawn(spec) {
            Err(SubagentError::BinaryNotFound { binary }) => {
                assert!(binary.contains("atomcode-no-such-bin"))
            }
            Err(other) => panic!("expected BinaryNotFound, got {other:?}"),
            Ok(_) => panic!("expected spawn to fail for a missing binary"),
        }
    }

    #[tokio::test]
    async fn timeout_kills_a_long_running_child() {
        let mut child = ManagedChild::spawn(sh("sleep 30")).unwrap();
        let outcome = child
            .wait_or_kill(Duration::from_millis(150), &CancellationToken::new())
            .await
            .unwrap();
        assert!(matches!(outcome, WaitOutcome::TimedOut), "got {outcome:?}");
    }

    #[tokio::test]
    async fn cancel_kills_a_long_running_child() {
        let mut child = ManagedChild::spawn(sh("sleep 30")).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = child
            .wait_or_kill(Duration::from_secs(5), &cancel)
            .await
            .unwrap();
        assert!(matches!(outcome, WaitOutcome::Cancelled), "got {outcome:?}");
    }

    #[tokio::test]
    async fn kill_tree_reaps_detached_grandchild() {
        // The shell backgrounds a long sleep (grandchild), prints its PID, then
        // sleeps itself. setsid + killpg must reap the grandchild that a
        // direct-child kill would orphan.
        let mut child = ManagedChild::spawn(sh("sleep 60 & echo $!; sleep 60")).unwrap();
        let stdout = child.take_stdout().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let gpid: i32 = lines
            .next_line()
            .await
            .unwrap()
            .expect("grandchild pid line")
            .trim()
            .parse()
            .expect("pid parse");

        child.kill_tree();
        // Give the signal a moment to propagate.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // kill(gpid, 0) → ESRCH once the grandchild is gone.
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let alive = unsafe { kill(gpid, 0) } == 0;
        assert!(
            !alive,
            "grandchild {gpid} should have been reaped by killpg"
        );
    }
}
