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

/// Build the `codex exec` argument vector (excludes the program itself). The
/// prompt is NOT an argument: it is piped on stdin (codex reads instructions
/// from stdin when no positional prompt is given), so a prompt beginning with
/// `-` can't be mis-parsed as a flag and the prompt never appears in `ps`.
fn codex_argv(
    permission: PermissionMode,
    model: Option<&str>,
    cwd: &Path,
    out_file: &Path,
) -> Vec<String> {
    let mut argv = vec![
        "exec".to_string(),
        "--skip-git-repo-check".to_string(),
        // JSONL events on stdout so we can stream meaningful activity (commands
        // run, searches, edits) while codex works, instead of a bare spinner.
        // The final answer still comes from the `-o` file (schema-stable).
        "--json".to_string(),
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

/// Turn one `codex exec --json` JSONL event line into a human progress line, or
/// `None` to skip it. Each thread item emits exactly once (at the phase where
/// its useful field is populated) to avoid started+completed duplicates; the
/// final `agent_message` is skipped (it is the answer, read from the `-o` file).
/// Defensive: any unparseable/unknown line yields `None`.
fn codex_activity_from_json_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let ev = v.get("type")?.as_str()?;
    if ev == "error" {
        let msg = v.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Some(format!("codex error: {}", clip(msg, 120)));
    }
    if ev != "item.started" && ev != "item.completed" {
        return None;
    }
    let item = v.get("item")?;
    let itype = item.get("type")?.as_str()?;
    let s = |k: &str| item.get(k).and_then(|x| x.as_str()).unwrap_or("");
    match (ev, itype) {
        ("item.started", "command_execution") => Some(format!("$ {}", clip(s("command"), 120))),
        ("item.started", "web_search") => Some(format!("web search: {}", clip(s("query"), 80))),
        ("item.started", "mcp_tool_call") => Some(format!("mcp: {}", s("tool"))),
        ("item.completed", "file_change") => {
            let n = item
                .get("changes")
                .and_then(|c| c.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            Some(format!("edited {n} file(s)"))
        }
        ("item.completed", "reasoning") => Some("thinking…".to_string()),
        _ => None,
    }
}

/// Extract the final `agent_message` text from a `codex exec --json` line, if
/// this line is one. Used as a FALLBACK for the `-o` file: if `-o` is empty
/// (older codex, `--json` interaction, or a crash before it flushed), the last
/// agent message from the stream is still the answer instead of a silent empty
/// result. Returns `None` for any other line.
fn codex_final_message_from_json_line(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "item.completed" {
        return None;
    }
    let item = v.get("item")?;
    if item.get("type")?.as_str()? != "agent_message" {
        return None;
    }
    item.get("text").and_then(|t| t.as_str()).map(str::to_string)
}

/// Truncate on a char boundary with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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
        let argv = codex_argv(self.permission, self.model.as_deref(), &req.cwd, &out.path);
        let mut spec = ChildSpec::new(&self.program, &req.cwd).args(argv);
        spec.stdin = super::proc::StdinMode::Piped;
        let mut child = ManagedChild::spawn(spec)?;
        // Prompt on stdin (not argv): avoids flag mis-parse and `ps` exposure.
        child.write_stdin_and_close(req.prompt.clone());

        // Parse the `--json` JSONL stream into meaningful activity lines (commands,
        // searches, edits, thinking) AND capture the last agent_message as a
        // fallback answer. The authoritative final answer comes from the `-o`
        // file; the captured message covers the case where `-o` is empty.
        // Unparseable lines are skipped.
        let mut final_msg = String::new();
        let (outcome, stderr_tail) =
            drain_and_wait(&mut child, self.timeout, &req.cancel, |line| {
                if let Some(activity) = codex_activity_from_json_line(&line) {
                    req.emit(SubagentEvent::Activity(activity));
                }
                if let Some(msg) = codex_final_message_from_json_line(&line) {
                    final_msg = msg;
                }
            })
            .await
            .map_err(|e| SubagentError::SpawnFailed(e.to_string()))?;

        // `-o` file first (schema-stable); fall back to the streamed agent_message
        // so a missing/empty `-o` never yields a silently empty answer.
        let output = |captured: &str| {
            let o = out.read();
            if o.trim().is_empty() {
                captured.to_string()
            } else {
                o
            }
        };

        match outcome {
            WaitOutcome::Exited(status) if status.success() => {
                Ok(result(output(&final_msg), SubagentStopReason::Completed))
            }
            WaitOutcome::Exited(status) => Err(SubagentError::NonZeroExit {
                code: status.code(),
                stderr_tail,
            }),
            WaitOutcome::TimedOut => Ok(result(output(&final_msg), SubagentStopReason::Timeout)),
            WaitOutcome::Cancelled => Ok(result(output(&final_msg), SubagentStopReason::Cancelled)),
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
        );
        assert_eq!(argv[0], "exec");
        assert!(argv.contains(&"--skip-git-repo-check".to_string()));
        assert!(argv.contains(&"--json".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--cd", "/proj"]));
        assert!(argv.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(argv.windows(2).any(|w| w == ["-o", "/tmp/out.txt"]));
        // Prompt is piped on stdin, never an argument.
        assert!(!argv.iter().any(|a| a == "fix it"));
        assert!(!argv.contains(&"--dangerously-bypass-approvals-and-sandbox".to_string()));
    }

    #[test]
    fn argv_accept_edits_and_auto_map_to_workspace_write() {
        for perm in [PermissionMode::AcceptEdits, PermissionMode::Auto] {
            let argv = codex_argv(perm, None, Path::new("/p"), Path::new("/o"));
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

    #[test]
    fn activity_parses_json_events_and_skips_noise() {
        // command_execution starts → "$ cmd".
        assert_eq!(
            codex_activity_from_json_line(
                r#"{"type":"item.started","item":{"id":"1","type":"command_execution","command":"cargo check"}}"#
            ),
            Some("$ cargo check".to_string())
        );
        // web_search / file_change / reasoning.
        assert_eq!(
            codex_activity_from_json_line(
                r#"{"type":"item.started","item":{"type":"web_search","query":"dead code"}}"#
            ),
            Some("web search: dead code".to_string())
        );
        assert_eq!(
            codex_activity_from_json_line(
                r#"{"type":"item.completed","item":{"type":"file_change","changes":[{},{}]}}"#
            ),
            Some("edited 2 file(s)".to_string())
        );
        assert_eq!(
            codex_activity_from_json_line(
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"…"}}"#
            ),
            Some("thinking…".to_string())
        );
        // agent_message (the final answer) is NOT emitted as activity.
        assert_eq!(
            codex_activity_from_json_line(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}"#
            ),
            None
        );
        // turn/thread noise + non-JSON → skipped; errors surface.
        assert_eq!(
            codex_activity_from_json_line(r#"{"type":"turn.started"}"#),
            None
        );
        assert_eq!(codex_activity_from_json_line("not json"), None);
        assert_eq!(codex_activity_from_json_line("  "), None);
        assert!(codex_activity_from_json_line(r#"{"type":"error","message":"boom"}"#)
            .unwrap()
            .contains("boom"));
    }

    #[test]
    fn final_message_extracts_only_agent_message_completed() {
        assert_eq!(
            codex_final_message_from_json_line(
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"the answer"}}"#
            ),
            Some("the answer".to_string())
        );
        // Not an agent_message / not completed / not JSON → None.
        assert_eq!(
            codex_final_message_from_json_line(
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"x"}}"#
            ),
            None
        );
        assert_eq!(
            codex_final_message_from_json_line(
                r#"{"type":"item.started","item":{"type":"agent_message","text":"x"}}"#
            ),
            None
        );
        assert_eq!(codex_final_message_from_json_line("not json"), None);
    }

    // Stub `codex`: emits `--json` JSONL events + writes the `-o` final message,
    // exits 0 — no real Codex, no network. Verifies spawn → JSONL parse → capture.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_captures_output_from_stub_codex() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-stub.sh");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
printf '{"type":"item.started","item":{"type":"command_execution","command":"ls crates"}}\n'
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; out="$1"; fi
  shift
done
printf 'STUB FINAL ANSWER' > "$out"
printf '{"type":"item.completed","item":{"type":"agent_message","text":"STUB FINAL ANSWER"}}\n'
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
        // The command_execution event became a "$ ls crates" activity line; the
        // agent_message (final answer) was NOT echoed as activity.
        assert!(
            activity.iter().any(|l| l == "$ ls crates"),
            "streamed activity: {activity:?}"
        );
        assert!(
            !activity.iter().any(|l| l.contains("STUB FINAL ANSWER")),
            "final answer must not be streamed as activity: {activity:?}"
        );
    }

    // Stub that emits an agent_message via JSONL but writes NO `-o` file → the
    // streamed message is the fallback answer (no silent-empty result).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_falls_back_to_stream_when_output_file_is_empty() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-noout.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"FALLBACK ANSWER\"}}\\n'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backend = CodexBackend::new("codex-noout", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_timeout(Duration::from_secs(10));
        let res = backend
            .run(SubagentRun::new("x", dir.path()))
            .await
            .unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Completed);
        assert_eq!(res.output, "FALLBACK ANSWER");
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

    // Stub that reads its stdin (the prompt) and echoes it into the `-o` final
    // answer. Verifies the end-to-end path: prompt is delivered on stdin (NOT an
    // argv flag — so it can't be mis-parsed as a flag or show up in `ps`), and
    // the `-o` file is the authoritative final answer.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_sends_prompt_on_stdin_and_uses_output_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-stdin.sh");
        std::fs::write(
            &stub,
            r#"#!/bin/sh
# Locate the -o path, then write stdin into it.
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; out="$1"; fi
  shift
done
cat > "$out"
exit 0
"#,
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexBackend::new("codex-stdin", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_timeout(Duration::from_secs(10));

        let prompt = "list all .rs files and summarize each";
        let res = backend
            .run(SubagentRun::new(prompt, dir.path()))
            .await
            .unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Completed);
        // The exact prompt bytes round-tripped through stdin into the answer.
        assert_eq!(res.output, prompt);
    }

    // Model override flows through as `-m <model>`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_passes_model_override_to_codex() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("codex-model.sh");
        // Record every argv into a temp file, then copy that into -o so the
        // test can assert the full argv. We use a side channel file because
        // `-o`'s value is consumed by the option parser and must not also be
        // treated as the output capture here.
        let capture = dir.path().join("argv.txt");
        let capture_path = capture.clone();
        std::fs::write(
            &stub,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$@" > "{cap}"
out=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; out="$1"; fi
  shift
done
cp "{cap}" "$out"
exit 0
"#,
                cap = capture_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = CodexBackend::new("codex-model", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_model("gpt-5-codex")
            .with_timeout(Duration::from_secs(10));

        let res = backend
            .run(SubagentRun::new("x", dir.path()))
            .await
            .unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Completed);
        assert!(
            res.output.contains("-m") && res.output.contains("gpt-5-codex"),
            "expected -m flag with model in argv, got: {:?}",
            res.output
        );
    }
}
