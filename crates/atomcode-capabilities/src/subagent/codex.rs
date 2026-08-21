//! Codex adapter: drives the `codex` CLI in non-interactive `exec` mode.
//!
//! MVP transport is `codex exec` (one-shot). The final agent message is captured
//! via `-o <file>` (a stable contract independent of the human/JSONL stdout
//! shape), while stdout lines are streamed best-effort as progress activity.
//! Multi-turn / streaming over `codex app-server --stdio` is Phase 4.
//!
//! Permission → flags (`codex exec` is non-interactive; the sandbox governs
//! autonomy, there is no on-request approval):
//! - `ReadOnly`    → `--sandbox read-only`
//! - `AcceptEdits` → `--sandbox workspace-write`
//! - `Auto`        → `--sandbox workspace-write`
//! - `Bypass`      → `--dangerously-bypass-approvals-and-sandbox` (no `--sandbox`)
//!
//! Phase 1 / T1.3 of the external-agent subagent-driver spec.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use super::proc::{drain_and_wait, ChildSpec, ManagedChild, WaitOutcome};
use super::{
    PermissionMode, SubagentBackend, SubagentCapabilities, SubagentError, SubagentEvent,
    SubagentKind, SubagentResult, SubagentRun, SubagentStopReason,
};

/// Default overall wall-clock ceiling for one delegated run.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// A named Codex instance driven via `codex exec`.
pub struct CodexBackend {
    name: String,
    /// Binary to invoke (default `codex`; overridable for tests).
    program: PathBuf,
    /// Optional `-m <model>`.
    model: Option<String>,
    /// Non-interactive permission posture.
    permission: PermissionMode,
    /// Whether `Bypass` is permitted; the assembly layer sets this false in
    /// non-interactive/scheduled contexts.
    allow_dangerous: bool,
    /// Overall run timeout.
    timeout: Duration,
}

impl CodexBackend {
    /// Construct a Codex backend for instance `name` with the given permission.
    pub fn new(name: impl Into<String>, permission: PermissionMode) -> Self {
        Self {
            name: name.into(),
            program: PathBuf::from("codex"),
            model: None,
            permission,
            allow_dangerous: false,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Override the binary path (tests point this at a stub).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Set the model (`-m`).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Permit `Bypass` (dangerous). Off by default.
    pub fn allow_dangerous(mut self, allow: bool) -> Self {
        self.allow_dangerous = allow;
        self
    }

    /// Override the run timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Map a permission mode to the Codex sandbox value (Bypass handled separately).
fn sandbox_for(permission: PermissionMode) -> &'static str {
    match permission {
        PermissionMode::ReadOnly => "read-only",
        PermissionMode::AcceptEdits | PermissionMode::Auto => "workspace-write",
        // Bypass never reaches here: it uses the bypass flag, not --sandbox.
        PermissionMode::Bypass => "read-only",
    }
}

/// Build the `codex exec` argument vector (excludes the program itself).
fn codex_argv(
    permission: PermissionMode,
    model: Option<&str>,
    cwd: &Path,
    out_file: &Path,
    prompt: &str,
) -> Vec<String> {
    let mut argv = vec![
        "exec".to_string(),
        "--skip-git-repo-check".to_string(),
        "--cd".to_string(),
        cwd.display().to_string(),
    ];
    if permission == PermissionMode::Bypass {
        argv.push("--dangerously-bypass-approvals-and-sandbox".to_string());
    } else {
        argv.push("--sandbox".to_string());
        argv.push(sandbox_for(permission).to_string());
    }
    if let Some(model) = model {
        argv.push("-m".to_string());
        argv.push(model.to_string());
    }
    argv.push("-o".to_string());
    argv.push(out_file.display().to_string());
    // Prompt LAST as the positional argument.
    argv.push(prompt.to_string());
    argv
}

/// A temp file path for `-o`; removed on drop. The file is created by codex, not
/// us — we only own the (unique) path and clean it up.
struct TempOutput {
    path: PathBuf,
}

impl TempOutput {
    fn new() -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("atomcode-codex-out-{pid}-{n}.txt"));
        Self { path }
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn result(output: String, stop_reason: SubagentStopReason) -> SubagentResult {
    SubagentResult { output, stop_reason }
}

#[async_trait]
impl SubagentBackend for CodexBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SubagentKind {
        SubagentKind::Codex
    }

    fn capabilities(&self) -> SubagentCapabilities {
        // MVP: no tool-filter / structured-output wiring yet.
        SubagentCapabilities::default()
    }

    async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError> {
        // Permission is a property of this named instance, not the call.
        if self.permission.is_dangerous() && !self.allow_dangerous {
            return Err(SubagentError::DangerousModeRefused);
        }
        let out = TempOutput::new();
        let argv = codex_argv(
            self.permission,
            self.model.as_deref(),
            &req.cwd,
            &out.path,
            &req.prompt,
        );
        let spec = ChildSpec::new(&self.program, &req.cwd).args(argv);
        let mut child = ManagedChild::spawn(spec)?;

        // Stream stdout as best-effort activity; the authoritative final answer
        // comes from the `-o` file, so nothing here needs to parse stdout.
        let (outcome, stderr_tail) =
            drain_and_wait(&mut child, self.timeout, &req.cancel, |line| {
                req.emit(SubagentEvent::Activity(line));
            })
            .await
            .map_err(|e| SubagentError::SpawnFailed(e.to_string()))?;

        match outcome {
            WaitOutcome::Exited(status) if status.success() => {
                Ok(result(out.read(), SubagentStopReason::Completed))
            }
            WaitOutcome::Exited(status) => Err(SubagentError::NonZeroExit {
                code: status.code(),
                stderr_tail,
            }),
            WaitOutcome::TimedOut => Ok(result(out.read(), SubagentStopReason::Timeout)),
            WaitOutcome::Cancelled => Ok(result(out.read(), SubagentStopReason::Cancelled)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn argv_read_only_uses_sandbox_read_only() {
        let argv = codex_argv(
            PermissionMode::ReadOnly,
            None,
            Path::new("/proj"),
            Path::new("/tmp/out.txt"),
            "fix it",
        );
        assert_eq!(argv[0], "exec");
        assert!(argv.contains(&"--skip-git-repo-check".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--cd", "/proj"]));
        assert!(argv.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(argv.windows(2).any(|w| w == ["-o", "/tmp/out.txt"]));
        assert_eq!(argv.last().unwrap(), "fix it", "prompt is the last positional");
        assert!(!argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn argv_accept_edits_and_auto_map_to_workspace_write() {
        for perm in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
            let argv = codex_argv(perm, None, Path::new("/p"), Path::new("/o"), "x");
            assert!(
                argv.windows(2).any(|w| w == ["--sandbox", "workspace-write"]),
                "{perm:?} → workspace-write"
            );
        }
    }

    #[test]
    fn argv_bypass_uses_bypass_flag_not_sandbox() {
        let argv = codex_argv(
            PermissionMode::Bypass,
            Some("gpt-5-codex"),
            Path::new("/p"),
            Path::new("/o"),
            "x",
        );
        assert!(argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
        assert!(!argv.contains(&"--sandbox".to_string()));
        assert!(argv.windows(2).any(|w| w == ["-m", "gpt-5-codex"]));
    }

    #[tokio::test]
    async fn bypass_refused_without_allow_dangerous() {
        let backend = CodexBackend::new("codex-x", PermissionMode::Bypass);
        let run = SubagentRun::new("do it", std::env::temp_dir());
        match backend.run(run).await {
            Err(SubagentError::DangerousModeRefused) => {}
            other => panic!("expected DangerousModeRefused, got {other:?}"),
        }
    }

    // Stub `codex`: a shell script that parses `-o <file>`, writes a final
    // message there, prints progress to stdout, and exits 0 — no real Codex,
    // no network. Verifies the full spawn → drain → capture path.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_captures_output_from_stub_codex() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-stub.sh");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
echo "progress: thinking"
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; out="$1"; fi
  shift
done
printf 'STUB FINAL ANSWER' > "$out"
echo "progress: done"
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexBackend::new("codex-stub", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_timeout(Duration::from_secs(10));

        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = std::sync::Arc::clone(&seen);
        let mut run = SubagentRun::new("summarize the repo", dir.path());
        run.on_event = Some(Box::new(move |ev| {
            if let SubagentEvent::Activity(l) = ev {
                sink_seen.lock().unwrap().push(l);
            }
        }));

        let res = backend.run(run).await.unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Completed);
        assert_eq!(res.output, "STUB FINAL ANSWER");
        let activity = seen.lock().unwrap().clone();
        assert!(activity.iter().any(|l| l.contains("thinking")), "streamed activity: {activity:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reports_non_zero_exit_with_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-fail.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho 'boom: codex blew up' 1>&2\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexBackend::new("codex-fail", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_timeout(Duration::from_secs(10));
        let run = SubagentRun::new("x", dir.path());
        match backend.run(run).await {
            Err(SubagentError::NonZeroExit { code, stderr_tail }) => {
                assert_eq!(code, Some(3));
                assert!(stderr_tail.contains("boom"), "stderr tail: {stderr_tail}");
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_cancel_returns_cancelled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-slow.sh");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexBackend::new("codex-slow", PermissionMode::ReadOnly)
            .with_program(&stub);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut run = SubagentRun::new("x", dir.path());
        run.cancel = cancel;
        let res = backend.run(run).await.unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Cancelled);
    }
}
