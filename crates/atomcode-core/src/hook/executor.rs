use std::time::Duration;

use tokio::process::Command;

use super::config::matching_hooks;
use super::{HookConfig, HookContext, HookEvent, PreHookResult};

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
    pub async fn run_pre_tool_use(
        &self,
        tool_name: &str,
        ctx: &HookContext,
    ) -> PreHookResult {
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
        let ctx_json =
            serde_json::to_string(ctx).unwrap_or_else(|_| "{}".to_string());

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(&hook.command)
            .env("ATOMCODE_HOOK_EVENT", &ctx.event)
            .env("ATOMCODE_HOOK_CONTEXT", &ctx_json);

        if let Some(ref name) = ctx.tool_name {
            cmd.env("ATOMCODE_TOOL_NAME", name);
        }

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
        let mut hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            "sleep 10",
        );
        hook.timeout_ms = 100; // 100 ms timeout
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    #[tokio::test]
    async fn hook_crash_degrades_to_allow() {
        let hook = make_hook(
            HookEvent::PreToolUse,
            Some("bash"),
            "exit 1",
        );
        let exec = HookExecutor::new(vec![hook]);
        let result = exec.run_pre_tool_use("bash", &test_ctx()).await;
        assert_eq!(result, PreHookResult::Allow);
    }

    // ── PostToolUse fire-and-forget ──────────────────────────────

    #[tokio::test]
    async fn post_tool_use_fire_and_forget() {
        let hook = make_hook(
            HookEvent::PostToolUse,
            Some("bash"),
            "echo done",
        );
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
