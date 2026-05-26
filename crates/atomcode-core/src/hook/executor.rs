use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::config::matching_hooks;
use super::{
    HookConfig, HookContext, HookEvent, PreHookResult, UserPromptHookResult,
    UserPromptSubmitOutput, UserPromptSubmitPayload,
};

/// Helper: append `extra` to `acc`, separated by a blank line. Keeps
/// hook-injected context blocks visually distinct when multiple hooks
/// each contribute a chunk.
fn push_context(acc: &mut String, extra: &str) {
    if !acc.is_empty() {
        acc.push_str("\n\n");
    }
    acc.push_str(extra);
}

/// Executes hook commands in response to agent lifecycle events.
pub struct HookExecutor {
    hooks: Vec<HookConfig>,
}

impl HookExecutor {
    /// Create an executor with the given hook configurations.
    pub fn new(hooks: Vec<HookConfig>) -> Self {
        Self { hooks }
    }

    /// Create an executor with no hooks (a no-op executor).
    pub fn empty() -> Self {
        Self { hooks: vec![] }
    }

    /// Whether any hooks are configured.
    pub fn has_hooks(&self) -> bool {
        !self.hooks.is_empty()
    }

    /// Run all matching `PreToolUse` hooks and return the aggregate result.
    ///
    /// If any hook returns `Block`, the overall result is `Block`.
    /// If any hook returns `Modify`, the last `Modify` wins.
    /// If a hook times out, crashes, or produces non-JSON output, it degrades
    /// to `Allow` (the tool call is not disrupted).
    pub async fn run_pre_tool_use(&self, tool_name: &str, ctx: &HookContext) -> PreHookResult {
        let matched = matching_hooks(&self.hooks, HookEvent::PreToolUse, Some(tool_name));
        if matched.is_empty() {
            return PreHookResult::Allow;
        }

        let mut result = PreHookResult::Allow;

        for hook in matched {
            match self.execute_hook(hook, ctx).await {
                Ok(stdout) => {
                    match serde_json::from_str::<PreHookResult>(&stdout) {
                        Ok(parsed) => match &parsed {
                            PreHookResult::Block { .. } => return parsed,
                            PreHookResult::Modify { .. } => result = parsed,
                            PreHookResult::Allow => {}
                        },
                        // Non-JSON output degrades to Allow.
                        Err(_) => {}
                    }
                }
                // Timeout or crash degrades to Allow.
                Err(_) => {}
            }
        }

        result
    }

    /// Run all matching `PostToolUse` hooks (fire-and-forget).
    ///
    /// Errors are silently swallowed — post-hooks are advisory.
    pub async fn run_post_tool_use(&self, tool_name: &str, ctx: &HookContext) {
        let matched = matching_hooks(&self.hooks, HookEvent::PostToolUse, Some(tool_name));
        for hook in matched {
            let _ = self.execute_hook(hook, ctx).await;
        }
    }

    /// Run every `UserPromptSubmit` hook in registration order. Aggregates
    /// results following CC's contract:
    ///
    /// - Any hook returning `decision: "block"` (or exit non-zero) → the
    ///   whole prompt is blocked; first reason wins.
    /// - Hooks emitting `hookSpecificOutput.additionalContext` (JSON) or
    ///   plain text on stdout → concatenated and surfaced to the agent as
    ///   extra context to append to the user message.
    /// - Empty stdout / unparseable JSON → treated as a silent continue,
    ///   so a hook author can still `print(...)` debug noise without
    ///   accidentally injecting it into every prompt.
    ///
    /// Each hook receives the payload as JSON on stdin (CC parity), so
    /// scripts using `json.load(sys.stdin)` work unchanged.
    pub async fn run_user_prompt_submit(
        &self,
        prompt: &str,
        session_id: &str,
        cwd: &str,
    ) -> UserPromptHookResult {
        let matched = matching_hooks(&self.hooks, HookEvent::UserPromptSubmit, None);
        if matched.is_empty() {
            return UserPromptHookResult::Continue;
        }

        let payload = UserPromptSubmitPayload {
            session_id: session_id.to_string(),
            hook_event_name: "UserPromptSubmit".to_string(),
            prompt: prompt.to_string(),
            cwd: cwd.to_string(),
        };
        let payload_json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());

        let mut injected = String::new();
        for hook in matched {
            match self.execute_hook_with_stdin(hook, &payload_json).await {
                Ok((exit_ok, stdout, stderr)) => {
                    if !exit_ok {
                        // Non-zero exit → block. Prefer stderr for the user
                        // message (CC convention: scripts use stderr for
                        // human-readable rejection text).
                        let reason = if !stderr.trim().is_empty() {
                            stderr.trim().to_string()
                        } else if !stdout.trim().is_empty() {
                            stdout.trim().to_string()
                        } else {
                            "user prompt blocked by hook".into()
                        };
                        return UserPromptHookResult::Block(reason);
                    }
                    // CC parity: hooks routinely log debug noise on
                    // earlier lines and emit the structured decision as
                    // the final line. If we parse the whole blob as JSON
                    // and fail (because of the debug noise), we MUST NOT
                    // fall through and inject the JSON as plain text —
                    // that silently turned `decision: "block"` into an
                    // inject in the previous version.
                    //
                    // Strategy: try the last non-empty line as JSON
                    // first. If it parses, act on it. Only then fall back
                    // to "treat full stdout as plain-text context".
                    let last_line = stdout.lines().rev().find(|l| !l.trim().is_empty());
                    let json_action = last_line.and_then(|l| {
                        serde_json::from_str::<UserPromptSubmitOutput>(l.trim()).ok()
                    });
                    if let Some(parsed) = json_action {
                        if matches!(parsed.decision.as_deref(), Some("block")) {
                            let reason = parsed
                                .reason
                                .unwrap_or_else(|| "user prompt blocked by hook".into());
                            return UserPromptHookResult::Block(reason);
                        }
                        if let Some(ctx) = parsed
                            .hook_specific_output
                            .and_then(|o| o.additional_context)
                        {
                            push_context(&mut injected, &ctx);
                            continue;
                        }
                        // Valid JSON but no actionable fields — silent continue.
                        continue;
                    }
                    // No JSON decision found anywhere in stdout: take the
                    // entire trimmed blob as plain-text additional context.
                    let trimmed = stdout.trim();
                    if !trimmed.is_empty() {
                        push_context(&mut injected, trimmed);
                    }
                }
                Err(_) => {
                    // Timeout / spawn failure degrades to continue, mirroring
                    // PreToolUse's fail-open behavior. The alternative —
                    // blocking every prompt on a flaky hook — is worse UX.
                }
            }
        }

        if injected.is_empty() {
            UserPromptHookResult::Continue
        } else {
            UserPromptHookResult::Inject(injected)
        }
    }

    /// Variant of `execute_hook` that pipes a payload to stdin and returns
    /// `(exit_ok, stdout, stderr)`. Used by event types that follow CC's
    /// stdin/stdout JSON protocol (currently only UserPromptSubmit).
    async fn execute_hook_with_stdin(
        &self,
        hook: &HookConfig,
        payload_json: &str,
    ) -> anyhow::Result<(bool, String, String)> {
        use std::process::Stdio;

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&hook.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(ref root) = hook.plugin_root {
            let s = root.as_os_str();
            cmd.env("CLAUDE_PLUGIN_ROOT", s);
            cmd.env("ATOMCODE_PLUGIN_ROOT", s);
        }
        crate::process_utils::suppress_console_window(&mut cmd);

        let timeout = Duration::from_millis(hook.timeout_ms);

        let fut = async {
            let mut child = cmd.spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(payload_json.as_bytes()).await?;
                // Explicit shutdown so the hook script's `read` /
                // `json.load(sys.stdin)` returns rather than hanging
                // until our timeout fires.
                stdin.shutdown().await.ok();
                drop(stdin);
            }
            let output = child.wait_with_output().await?;
            anyhow::Ok((
                output.status.success(),
                String::from_utf8_lossy(&output.stdout).to_string(),
                String::from_utf8_lossy(&output.stderr).to_string(),
            ))
        };

        Ok(tokio::time::timeout(timeout, fut).await??)
    }

    /// Run all hooks matching a session-level event (fire-and-forget).
    pub async fn run_session_event(&self, event: HookEvent, ctx: &HookContext) {
        let matched = matching_hooks(&self.hooks, event, None);
        for hook in matched {
            let _ = self.execute_hook(hook, ctx).await;
        }
    }

    /// Execute a single hook command and return its stdout.
    ///
    /// The hook receives context via environment variables:
    /// - `ATOMCODE_HOOK_EVENT`   — the event name (e.g. `pre_tool_use`)
    /// - `ATOMCODE_TOOL_NAME`    — tool name, if applicable
    /// - `ATOMCODE_HOOK_CONTEXT` — full JSON-serialized `HookContext`
    ///
    /// The command is killed after `hook.timeout_ms` milliseconds.
    pub async fn execute_hook(
        &self,
        hook: &HookConfig,
        ctx: &HookContext,
    ) -> anyhow::Result<String> {
        let ctx_json = serde_json::to_string(ctx).unwrap_or_else(|_| "{}".to_string());

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&hook.command)
            .env("ATOMCODE_HOOK_EVENT", &ctx.event)
            .env("ATOMCODE_HOOK_CONTEXT", &ctx_json);

        if let Some(ref name) = ctx.tool_name {
            cmd.env("ATOMCODE_TOOL_NAME", name);
        }
        if let Some(ref root) = hook.plugin_root {
            // CC parity: scripts reference `"${CLAUDE_PLUGIN_ROOT}/foo"`
            // via shell expansion. Mirroring under both names keeps
            // atomcode-native plugins idiomatic too.
            let s = root.as_os_str();
            cmd.env("CLAUDE_PLUGIN_ROOT", s);
            cmd.env("ATOMCODE_PLUGIN_ROOT", s);
        }

        crate::process_utils::suppress_console_window(&mut cmd);

        let timeout = Duration::from_millis(hook.timeout_ms);

        let output = tokio::time::timeout(timeout, cmd.output()).await??;

        if !output.status.success() {
            anyhow::bail!(
                "hook command exited with status {}",
                output.status.code().unwrap_or(-1)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_ctx() -> HookContext {
        HookContext {
            event: "pre_tool_use".into(),
            tool_name: Some("bash".into()),
            tool_args: Some(json!({"command": "ls"})),
            tool_result: None,
            tool_success: None,
            session_id: "test-session".into(),
            working_dir: "/tmp".into(),
        }
    }

    fn make_hook(event: HookEvent, matcher: Option<&str>, cmd: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: matcher.map(String::from),
            command: cmd.to_string(),
            timeout_ms: 10_000,
            plugin_root: None,
        }
    }

    // ── Basic executor ───────────────────────────────────────────

    #[tokio::test]
    async fn empty_executor_allows() {
        let exec = HookExecutor::empty();
        assert!(!exec.has_hooks());
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── PreToolUse result parsing ────────────────────────────────

    #[tokio::test]
    async fn hook_returning_allow_json() {
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            r#"echo '{"action":"allow"}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    #[tokio::test]
    async fn hook_returning_block_json() {
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            r#"echo '{"action":"block","reason":"dangerous"}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(
            result,
            PreHookResult::Block {
                reason: "dangerous".into()
            }
        );
    }

    #[tokio::test]
    async fn hook_returning_non_json_allows() {
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            "echo 'not json at all'",
        );
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── Error conditions ─────────────────────────────────────────

    #[tokio::test]
    async fn hook_timeout_degrades_to_allow() {
        let mut hook = make_hook(HookEvent::PreToolUse, Some("bash"), "sleep 10");
        hook.timeout_ms = 100; // 100 ms timeout
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── UserPromptSubmit ─────────────────────────────────────────

    #[tokio::test]
    async fn user_prompt_no_hooks_returns_continue() {
        let exec = HookExecutor::empty();
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Continue);
    }

    #[tokio::test]
    async fn user_prompt_plain_stdout_injects_context() {
        let hook = make_hook(HookEvent::UserPromptSubmit, None, "echo extra-info");
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Inject("extra-info".into()));
    }

    #[tokio::test]
    async fn user_prompt_decision_block_blocks() {
        let hook = make_hook(
            HookEvent::UserPromptSubmit,
            None,
            r#"echo '{"decision":"block","reason":"nope"}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Block("nope".into()));
    }

    #[tokio::test]
    async fn user_prompt_hook_specific_output_injects() {
        let hook = make_hook(
            HookEvent::UserPromptSubmit,
            None,
            r#"echo '{"hookSpecificOutput":{"additionalContext":"ctx-bag"}}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Inject("ctx-bag".into()));
    }

    #[tokio::test]
    async fn user_prompt_nonzero_exit_blocks_with_stderr() {
        let hook = make_hook(HookEvent::UserPromptSubmit, None, "echo bad >&2; exit 1");
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Block("bad".into()));
    }

    /// Regression: stdout that mixes debug logging with a trailing JSON
    /// decision used to fail the whole-blob `serde_json::from_str` and
    /// fall through to plain-text injection — silently turning `block`
    /// into `inject`. Last-line parse must catch the JSON.
    #[tokio::test]
    async fn user_prompt_block_after_debug_noise_still_blocks() {
        let hook = make_hook(
            HookEvent::UserPromptSubmit,
            None,
            r#"echo 'debug line 1'; echo 'debug line 2'; echo '{"decision":"block","reason":"final"}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Block("final".into()));
    }

    /// Plugin-root path containing a space MUST NOT break the hook command.
    /// We pass it as `CLAUDE_PLUGIN_ROOT` env var; the hook expands it
    /// inside its own quoted reference.
    #[tokio::test]
    async fn user_prompt_plugin_root_with_spaces_via_env() {
        // Hook reads `$CLAUDE_PLUGIN_ROOT` (set by the executor) and
        // echoes it back. Path contains a space to prove we are not
        // doing string substitution.
        let mut hook = make_hook(
            HookEvent::UserPromptSubmit,
            None,
            r#"printf '%s' "$CLAUDE_PLUGIN_ROOT""#,
        );
        hook.plugin_root = Some(std::path::PathBuf::from("/opt/has space/x"));
        let exec = HookExecutor::new(vec![hook]);
        let r = exec.run_user_prompt_submit("hi", "s", "/tmp").await;
        assert_eq!(r, UserPromptHookResult::Inject("/opt/has space/x".into()));
    }

    #[tokio::test]
    async fn user_prompt_payload_reaches_stdin() {
        // Hook reads stdin and echoes the prompt field back; we verify the
        // payload made it through and was valid JSON with the expected key.
        let hook = make_hook(
            HookEvent::UserPromptSubmit,
            None,
            r#"python3 -c 'import json,sys;d=json.load(sys.stdin);print(d["prompt"])'"#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let r = exec
            .run_user_prompt_submit("ping-payload", "sess", "/tmp")
            .await;
        assert_eq!(r, UserPromptHookResult::Inject("ping-payload".into()));
    }

    #[tokio::test]
    async fn hook_crash_degrades_to_allow() {
        let hook = make_hook(HookEvent::PreToolUse, Some("bash"), "exit 1");
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── PostToolUse fire-and-forget ──────────────────────────────

    #[tokio::test]
    async fn post_tool_use_fire_and_forget() {
        let hook = make_hook(HookEvent::PostToolUse, Some("bash"), "echo done");
        let exec = HookExecutor::new(vec![hook]);
        // Should not panic or propagate errors.
        exec.run_post_tool_use("bash", &test_ctx()).await;
    }

    // ── Matcher integration ──────────────────────────────────────

    #[tokio::test]
    async fn matcher_filters_correctly() {
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            r#"echo '{"action":"block","reason":"bash only"}'"#,
        );
        let exec = HookExecutor::new(vec![hook]);

        // Should block for bash
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(
            result,
            PreHookResult::Block {
                reason: "bash only".into()
            }
        );

        // Should allow for grep (hook doesn't match)
        let result = exec.run_pre_tool_use("grep", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── Environment variables ────────────────────────────────────

    #[tokio::test]
    async fn hook_receives_env_vars() {
        // The hook echoes environment variables as JSON so we can verify.
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            r#"printf '{"event":"%s","tool":"%s","has_ctx":"%s"}' "$ATOMCODE_HOOK_EVENT" "$ATOMCODE_TOOL_NAME" "$(test -n "$ATOMCODE_HOOK_CONTEXT" && echo yes || echo no)""#,
        );
        let exec = HookExecutor::new(vec![hook]);
        let ctx = test_ctx();

        // We don't care about the PreHookResult (it won't be valid JSON for
        // our PreHookResult enum), so call execute_hook directly.
        let stdout = exec.execute_hook(&exec.hooks[0], &ctx).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();

        assert_eq!(parsed["event"], "pre_tool_use");
        assert_eq!(parsed["tool"], "bash");
        assert_eq!(parsed["has_ctx"], "yes");
    }
}
