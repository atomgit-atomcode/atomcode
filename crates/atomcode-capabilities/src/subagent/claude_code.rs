//! Claude Code adapter: drives the `claude` CLI in headless print mode.
//!
//! Transport is `claude -p --output-format json` (one-shot). The single JSON
//! result object is parsed for its `result` field — a stable contract that does
//! not depend on the incremental stream-json event shape. Live activity
//! streaming (stream-json) is a later improvement.
//!
//! Permission → flags (`claude --permission-mode` choices: acceptEdits, auto,
//! bypassPermissions, default, dontAsk, plan):
//! - `ReadOnly`    → `--permission-mode plan` (plan mode makes no edits)
//! - `AcceptEdits` → `--permission-mode acceptEdits`
//! - `Auto`        → `--permission-mode auto`
//! - `Bypass`      → `--dangerously-skip-permissions`
//!
//! Phase 1 / T1.4 of the external-agent subagent-driver spec.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use super::proc::{drain_and_wait, ChildSpec, ManagedChild, WaitOutcome};
use super::{
    PermissionMode, SubagentBackend, SubagentCapabilities, SubagentError, SubagentKind,
    SubagentResult, SubagentRun, SubagentStopReason,
};

/// Default overall wall-clock ceiling for one delegated run.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// A named Claude Code instance driven via `claude -p`.
pub struct ClaudeCodeBackend {
    name: String,
    program: PathBuf,
    model: Option<String>,
    permission: PermissionMode,
    allow_dangerous: bool,
    timeout: Duration,
}

impl ClaudeCodeBackend {
    /// Construct a Claude Code backend for instance `name`.
    pub fn new(name: impl Into<String>, permission: PermissionMode) -> Self {
        Self {
            name: name.into(),
            program: PathBuf::from("claude"),
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

    /// Set the model (`--model`).
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

/// The `--permission-mode` value for a non-bypass mode.
fn permission_mode_flag(permission: PermissionMode) -> &'static str {
    match permission {
        PermissionMode::ReadOnly => "plan",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Auto => "auto",
        // Bypass never reaches here: it uses --dangerously-skip-permissions.
        PermissionMode::Bypass => "plan",
    }
}

/// Build the `claude` argument vector (excludes the program itself). The agent
/// runs in the process cwd (set on the child). The prompt is NOT an argument: it
/// is piped on stdin (claude reads it with the default `text` input format), so
/// a prompt beginning with `-` can't be mis-parsed as a flag and never appears
/// in `ps`.
fn claude_argv(permission: PermissionMode, model: Option<&str>) -> Vec<String> {
    let mut argv = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
    ];
    if permission == PermissionMode::Bypass {
        argv.push("--dangerously-skip-permissions".to_string());
    } else {
        argv.push("--permission-mode".to_string());
        argv.push(permission_mode_flag(permission).to_string());
    }
    if let Some(model) = model {
        argv.push("--model".to_string());
        argv.push(model.to_string());
    }
    argv
}

/// The parsed outcome of a `claude --output-format json` result object.
struct ClaudeOutcome {
    /// The assistant's final text (or a best-effort fallback).
    text: String,
    /// The agent reported its own failure (`is_error: true`).
    is_error: bool,
}

/// Parse the single `claude --output-format json` result object. Extracts the
/// `result` string and the `is_error` flag; falls back to the raw buffer for the
/// text when the payload cannot be parsed or `result` is absent/non-string
/// (defensive against schema drift — never drop the output entirely).
fn parse_result(buf: &str) -> ClaudeOutcome {
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        return ClaudeOutcome {
            text: String::new(),
            is_error: false,
        };
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
            let text = v
                .get("result")
                .and_then(|r| r.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    // No string `result`: prefer the subtype (e.g. error_max_turns)
                    // over dumping the whole JSON envelope at the model.
                    v.get("subtype")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| trimmed.to_string())
                });
            ClaudeOutcome { text, is_error }
        }
        Err(_) => ClaudeOutcome {
            text: trimmed.to_string(),
            is_error: false,
        },
    }
}

fn result(output: String, stop_reason: SubagentStopReason) -> SubagentResult {
    SubagentResult {
        output,
        stop_reason,
    }
}

#[async_trait]
impl SubagentBackend for ClaudeCodeBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> SubagentKind {
        SubagentKind::ClaudeCode
    }

    fn capabilities(&self) -> SubagentCapabilities {
        // MVP: no tool-filter / structured-output wiring yet. (claude supports
        // --allowedTools; wiring is a later task.)
        SubagentCapabilities::default()
    }

    async fn run(&self, req: SubagentRun) -> Result<SubagentResult, SubagentError> {
        if self.permission.is_dangerous() && !self.allow_dangerous {
            return Err(SubagentError::DangerousModeRefused);
        }
        let argv = claude_argv(self.permission, self.model.as_deref());
        let mut spec = ChildSpec::new(&self.program, &req.cwd).args(argv);
        spec.stdin = super::proc::StdinMode::Piped;
        let mut child = ManagedChild::spawn(spec)?;
        // Prompt on stdin (not argv): avoids flag mis-parse and `ps` exposure.
        child.write_stdin_and_close(req.prompt.clone());

        // `--output-format json` emits a single result object; buffer stdout and
        // parse at the end (do NOT stream raw JSON as activity).
        let mut buf = String::new();
        let (outcome, stderr_tail) =
            drain_and_wait(&mut child, self.timeout, &req.cancel, |line| {
                buf.push_str(&line);
                buf.push('\n');
            })
            .await
            .map_err(|e| SubagentError::SpawnFailed(e.to_string()))?;

        match outcome {
            WaitOutcome::Exited(status) if status.success() => {
                let parsed = parse_result(&buf);
                // claude can exit 0 yet report its own failure (is_error): surface
                // it as an agent-level error, not a successful completion.
                if parsed.is_error {
                    Err(SubagentError::AgentError(parsed.text))
                } else {
                    Ok(result(parsed.text, SubagentStopReason::Completed))
                }
            }
            WaitOutcome::Exited(status) => Err(SubagentError::NonZeroExit {
                code: status.code(),
                stderr_tail,
            }),
            WaitOutcome::TimedOut => {
                Ok(result(parse_result(&buf).text, SubagentStopReason::Timeout))
            }
            WaitOutcome::Cancelled => Ok(result(
                parse_result(&buf).text,
                SubagentStopReason::Cancelled,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn argv_read_only_uses_plan_mode() {
        let argv = claude_argv(PermissionMode::ReadOnly, None);
        assert!(argv.windows(2).any(|w| w == ["-p", "--output-format"]));
        assert!(argv.windows(2).any(|w| w == ["--output-format", "json"]));
        assert!(argv.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        // Prompt is piped on stdin, never an argument.
        assert!(!argv.iter().any(|a| a == "fix it"));
        assert!(!argv.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn argv_modes_map_correctly() {
        assert!(claude_argv(PermissionMode::AcceptEdits, None)
            .windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]));
        assert!(claude_argv(PermissionMode::Auto, None)
            .windows(2)
            .any(|w| w == ["--permission-mode", "auto"]));
    }

    #[test]
    fn argv_bypass_uses_skip_permissions_not_mode() {
        let argv = claude_argv(PermissionMode::Bypass, Some("opus"));
        assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(!argv.contains(&"--permission-mode".to_string()));
        assert!(argv.windows(2).any(|w| w == ["--model", "opus"]));
    }

    #[test]
    fn parse_result_extracts_result_and_error_flag() {
        let ok = parse_result(
            r#"{"type":"result","subtype":"success","result":"THE ANSWER","is_error":false}"#,
        );
        assert_eq!(ok.text, "THE ANSWER");
        assert!(!ok.is_error);
        // is_error true with a null result → surfaces the subtype, flags error,
        // does NOT dump the raw JSON envelope.
        let e = parse_result(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":null}"#,
        );
        assert!(e.is_error);
        assert_eq!(e.text, "error_max_turns");
        // Non-JSON → raw passthrough, not an error.
        let raw = parse_result("plain text out");
        assert_eq!(raw.text, "plain text out");
        assert!(!raw.is_error);
        assert_eq!(parse_result("   ").text, "");
    }

    #[tokio::test]
    async fn bypass_refused_without_allow_dangerous() {
        let backend = ClaudeCodeBackend::new("cc-x", PermissionMode::Bypass);
        let run = SubagentRun::new("do it", std::env::temp_dir());
        match backend.run(run).await {
            Err(SubagentError::DangerousModeRefused) => {}
            other => panic!("expected DangerousModeRefused, got {other:?}"),
        }
    }

    // Stub `claude`: prints one JSON result object, exits 0. No real Claude, no
    // network. Verifies spawn → buffer → parse → result.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_parses_json_result_from_stub_claude() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-stub.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"CLAUDE ANSWER\",\"is_error\":false}'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend = ClaudeCodeBackend::new("cc-stub", PermissionMode::ReadOnly)
            .with_program(&stub)
            .with_timeout(Duration::from_secs(10));
        let run = SubagentRun::new("summarize", dir.path());
        let res = backend.run(run).await.unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Completed);
        assert_eq!(res.output, "CLAUDE ANSWER");
    }

    // claude exits 0 but reports is_error → surfaced as AgentError, not Completed.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_maps_is_error_to_agent_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-agenterr.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\nprintf '{\"type\":\"result\",\"subtype\":\"error_max_turns\",\"is_error\":true,\"result\":null}'\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backend =
            ClaudeCodeBackend::new("cc-agenterr", PermissionMode::ReadOnly).with_program(&stub);
        match backend.run(SubagentRun::new("x", dir.path())).await {
            Err(SubagentError::AgentError(msg)) => assert!(msg.contains("error_max_turns")),
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_reports_non_zero_exit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-fail.sh");
        std::fs::write(&stub, "#!/bin/sh\necho 'cc error' 1>&2\nexit 2\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend =
            ClaudeCodeBackend::new("cc-fail", PermissionMode::ReadOnly).with_program(&stub);
        let run = SubagentRun::new("x", dir.path());
        match backend.run(run).await {
            Err(SubagentError::NonZeroExit { code, stderr_tail }) => {
                assert_eq!(code, Some(2));
                assert!(stderr_tail.contains("cc error"));
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_cancel_returns_cancelled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-slow.sh");
        std::fs::write(&stub, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let backend =
            ClaudeCodeBackend::new("cc-slow", PermissionMode::ReadOnly).with_program(&stub);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut run = SubagentRun::new("x", dir.path());
        run.cancel = cancel;
        let res = backend.run(run).await.unwrap();
        assert_eq!(res.stop_reason, SubagentStopReason::Cancelled);
    }
}
