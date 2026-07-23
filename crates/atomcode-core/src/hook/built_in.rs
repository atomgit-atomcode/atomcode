//! 内置工程化 Hook 实现
//!
//! 这些 hook 提供了常见的工程化能力：
//! - 工具调用审计日志
//! - 自动 Git 提交
//! - 会话统计报告
//! - 错误上报
//! - 模型响应验证

use async_trait::async_trait;
use chrono::Local;

use crate::hook::{
    ErrorContext, Hook, HookResult, OnErrorHook, OnModelResponseHook, OnSessionEndHook,
    OnSessionStartHook, OnToolCallStartHook, OnTurnCompleteHook, OnTurnStartHook, SessionContext,
    ToolCallStartContext, TurnCompleteContext, TurnStartContext,
};

// ============================================================================
// 1. 工具调用审计日志 Hook
// ============================================================================

/// 记录所有工具调用到日志文件
pub struct ToolAuditLogHook {
    /// 是否启用
    pub enabled: bool,
    /// 日志文件路径（可选，默认输出到 stderr）
    pub log_file: Option<std::path::PathBuf>,
}

impl Hook for ToolAuditLogHook {
    fn name(&self) -> &str {
        "tool-audit-log"
    }

    fn description(&self) -> &str {
        "记录所有工具调用到审计日志"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnToolCallStartHook for ToolAuditLogHook {
    async fn on_tool_call_start(&self, ctx: &ToolCallStartContext) -> HookResult {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        let log_entry = format!(
            "[{}] TURN #{} | Tool: {} | CallID: {}\n",
            timestamp, ctx.turn_number, ctx.tool_name, ctx.call_id
        );

        if let Some(ref log_file) = self.log_file {
            // 追加到日志文件
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
            {
                let _ = file.write_all(log_entry.as_bytes());
            }
        } else {
            // 输出到 tracing（stderr 在 TUI alternate screen 下会污染输入栏）
            tracing::info!("{}", log_entry.trim_end());
        }

        HookResult::Ok
    }
}

// ============================================================================
// 2. Turn 统计 Hook
// ============================================================================

/// 收集每个 Turn 的统计信息
pub struct TurnStatsHook {
    pub enabled: bool,
}

impl Hook for TurnStatsHook {
    fn name(&self) -> &str {
        "turn-stats"
    }

    fn description(&self) -> &str {
        "收集 Turn 级别的统计信息"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnTurnStartHook for TurnStatsHook {
    async fn on_turn_start(&self, _ctx: &TurnStartContext) -> HookResult {
        // Turn 开始时记录
        HookResult::Ok
    }
}

#[async_trait]
impl OnTurnCompleteHook for TurnStatsHook {
    async fn on_turn_complete(&self, ctx: &TurnCompleteContext) -> HookResult {
        tracing::info!(
            "[Turn #{}] Result: {} | Tokens: {} | Tools: {} | Duration: {}ms | Files: {:?}",
            ctx.turn_number,
            ctx.result_type,
            ctx.tokens_used,
            ctx.tool_calls,
            ctx.duration_ms,
            ctx.edited_files
        );
        HookResult::Ok
    }
}

// ============================================================================
// 3. 自动 Git 提交 Hook
// ============================================================================

/// 每 N 个 Turn 自动提交一次
pub struct AutoCommitHook {
    pub enabled: bool,
    /// 每多少个 Turn 提交一次
    pub interval: u32,
}

impl Hook for AutoCommitHook {
    fn name(&self) -> &str {
        "auto-commit"
    }

    fn description(&self) -> &str {
        "定期自动提交代码变更"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnTurnCompleteHook for AutoCommitHook {
    async fn on_turn_complete(&self, ctx: &TurnCompleteContext) -> HookResult {
        // 只在达到间隔时提交
        if ctx.turn_number % self.interval != 0 {
            return HookResult::Ok;
        }

        // 检查是否有文件变更
        if ctx.edited_files.is_empty() {
            return HookResult::Ok;
        }

        // 执行 git add 和 commit
        self.run_git_commit(ctx).await
    }
}

impl AutoCommitHook {
    async fn run_git_commit(&self, ctx: &TurnCompleteContext) -> HookResult {
        // 检查是否有未提交的变更
        let output = match std::process::Command::new("git")
            .args(&["status", "--porcelain"])
            .output()
        {
            Ok(o) => o,
            Err(e) => return HookResult::Warning(format!("Failed to run git status: {}", e)),
        };

        // 如果没有变更，跳过
        if output.stdout.is_empty() {
            return HookResult::Ok;
        }

        // git add -A
        let _ = std::process::Command::new("git")
            .args(&["add", "-A"])
            .output();

        // 统计实际 stage 的文件数（git add -A 可能添加了比系统追踪更多的文件）
        let file_count = match std::process::Command::new("git")
            .args(&["diff", "--cached", "--name-only"])
            .output()
        {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let count = stdout.lines().filter(|l| !l.is_empty()).count();
                if count > 0 {
                    count
                } else {
                    ctx.edited_files.len()
                }
            }
            Err(_) => ctx.edited_files.len(),
        };

        // git commit
        let commit_msg = format!(
            "Auto-commit at turn #{} ({} files changed)",
            ctx.turn_number, file_count
        );

        match std::process::Command::new("git")
            .args(&["commit", "-m", &commit_msg])
            .output()
        {
            Ok(output) if output.status.success() => {
                tracing::info!("[AutoCommit] Committed at turn #{}", ctx.turn_number);
                HookResult::Ok
            }
            Ok(output) => HookResult::Warning(format!(
                "Git commit failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )),
            Err(e) => HookResult::Warning(format!("Failed to run git commit: {}", e)),
        }
    }
}

// ============================================================================
// 4. 会话总结 Hook
// ============================================================================

/// 在会话结束时生成总结报告
pub struct SessionSummaryHook {
    pub enabled: bool,
}

impl SessionSummaryHook {
    pub fn new() -> Self {
        Self { enabled: true }
    }
}

impl Hook for SessionSummaryHook {
    fn name(&self) -> &str {
        "session-summary"
    }

    fn description(&self) -> &str {
        "在会话结束时生成总结报告"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnSessionStartHook for SessionSummaryHook {
    async fn on_session_start(&self, _ctx: &SessionContext) -> HookResult {
        // 重置为可变状态（实际实现需要内部可变性）
        HookResult::Ok
    }
}

#[async_trait]
impl OnSessionEndHook for SessionSummaryHook {
    async fn on_session_end(&self, ctx: &SessionContext) -> HookResult {
        let sep = "=".repeat(60);
        tracing::info!(
            "\n{sep}\n[Session Summary]\nSession ID: {}\nWorking Dir: {}\nModel: {} ({})\n{sep}",
            ctx.session_id,
            ctx.working_dir,
            ctx.model_name,
            ctx.provider_name,
        );

        HookResult::Ok
    }
}

// ============================================================================
// 5. 错误上报 Hook
// ============================================================================

/// 错误发生时记录详细信息
pub struct ErrorReportHook {
    pub enabled: bool,
}

impl Hook for ErrorReportHook {
    fn name(&self) -> &str {
        "error-report"
    }

    fn description(&self) -> &str {
        "记录错误详细信息到日志"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnErrorHook for ErrorReportHook {
    async fn on_error(&self, ctx: &ErrorContext) -> HookResult {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        tracing::error!(
            "[ERROR REPORT] {}\nType: {}\nPhase: {}\nTurn: {:?}\nMessage: {}",
            timestamp,
            ctx.error_type,
            ctx.phase,
            ctx.turn_number,
            ctx.error_message
        );

        HookResult::Ok
    }
}

// ============================================================================
// 6. 模型响应验证 Hook
// ============================================================================

/// 验证模型响应是否包含敏感信息
pub struct ResponseValidationHook {
    pub enabled: bool,
    /// 敏感词列表
    sensitive_patterns: Vec<String>,
}

impl ResponseValidationHook {
    pub fn new(patterns: Vec<String>) -> Self {
        Self {
            enabled: true,
            sensitive_patterns: patterns,
        }
    }
}

impl Hook for ResponseValidationHook {
    fn name(&self) -> &str {
        "response-validation"
    }

    fn description(&self) -> &str {
        "验证模型响应，检测敏感信息泄露"
    }

    fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[async_trait]
impl OnModelResponseHook for ResponseValidationHook {
    async fn on_model_response(&self, response: &str, _turn_ctx: &TurnStartContext) -> HookResult {
        let response_lower = response.to_lowercase();

        for pattern in &self.sensitive_patterns {
            if response_lower.contains(&pattern.to_lowercase()) {
                return HookResult::Warning(format!(
                    "Response may contain sensitive information: '{}'",
                    pattern
                ));
            }
        }

        HookResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolAuditLogHook ─────────────────────────────────────────────

    #[test]
    fn test_tool_audit_log_hook_trait() {
        let hook = ToolAuditLogHook {
            enabled: true,
            log_file: None,
        };
        assert_eq!(hook.name(), "tool-audit-log");
        assert!(hook.is_enabled());

        let hook_disabled = ToolAuditLogHook {
            enabled: false,
            log_file: None,
        };
        assert!(!hook_disabled.is_enabled());
    }

    #[tokio::test]
    async fn test_tool_audit_log_on_tool_call_start() {
        let hook = ToolAuditLogHook {
            enabled: true,
            log_file: None,
        };
        let ctx = ToolCallStartContext {
            tool_name: "bash".into(),
            tool_args: "echo hello".into(),
            call_id: "call_123".into(),
            turn_number: 1,
        };
        let result = hook.on_tool_call_start(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── TurnStatsHook ────────────────────────────────────────────────

    #[test]
    fn test_turn_stats_hook_trait() {
        let hook = TurnStatsHook { enabled: true };
        assert_eq!(hook.name(), "turn-stats");
        assert!(hook.is_enabled());
    }

    #[tokio::test]
    async fn test_turn_stats_on_turn_start_and_complete() {
        let hook = TurnStatsHook { enabled: true };
        let ctx = TurnStartContext {
            turn_number: 1,
            session_id: Some("test-session".into()),
            working_dir: "/tmp".into(),
            phase: "execution".into(),
            has_file_context: false,
        };
        let result = hook.on_turn_start(&ctx).await;
        assert!(matches!(result, HookResult::Ok));

        let complete_ctx = TurnCompleteContext {
            turn_number: 1,
            result_type: "Responded".into(),
            tokens_used: 500,
            tool_calls: 3,
            duration_ms: 1500,
            truncated: false,
            edited_files: vec!["a.rs".into(), "b.rs".into()],
        };
        let result = hook.on_turn_complete(&complete_ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── AutoCommitHook ───────────────────────────────────────────────

    #[test]
    fn test_auto_commit_hook_trait() {
        let hook = AutoCommitHook {
            enabled: true,
            interval: 1,
        };
        assert!(hook.is_enabled());

        let hook_disabled = AutoCommitHook {
            enabled: false,
            interval: 1,
        };
        assert!(!hook_disabled.is_enabled());
    }

    #[tokio::test]
    async fn test_auto_commit_no_changes() {
        let hook = AutoCommitHook {
            enabled: true,
            interval: 1,
        };
        let ctx = TurnCompleteContext {
            turn_number: 1,
            result_type: "Responded".into(),
            tokens_used: 100,
            tool_calls: 1,
            duration_ms: 100,
            truncated: false,
            edited_files: vec![],
        };
        let result = hook.run_git_commit(&ctx).await;
        // In a real repo this may attempt a git commit; verify no panic
        match result {
            HookResult::Ok | HookResult::Warning(_) => {}
            other => panic!("Unexpected result: {:?}", other),
        }
    }

    // ── SessionSummaryHook ───────────────────────────────────────────

    #[test]
    fn test_session_summary_hook_trait() {
        let hook = SessionSummaryHook::new();
        assert_eq!(hook.name(), "session-summary");
        assert!(hook.is_enabled());
    }

    #[tokio::test]
    async fn test_session_summary_start_end() {
        let hook = SessionSummaryHook::new();
        let ctx = SessionContext {
            session_id: "session-1".into(),
            working_dir: "/tmp".into(),
            model_name: "deepseek-v4".into(),
            provider_name: "atomgit".into(),
        };
        let result = hook.on_session_start(&ctx).await;
        assert!(matches!(result, HookResult::Ok));

        let result = hook.on_session_end(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── ErrorReportHook ──────────────────────────────────────────────

    #[test]
    fn test_error_report_hook_trait() {
        let hook = ErrorReportHook { enabled: true };
        assert_eq!(hook.name(), "error-report");
        assert!(hook.is_enabled());
    }

    #[tokio::test]
    async fn test_error_report_on_error() {
        let hook = ErrorReportHook { enabled: true };
        let ctx = ErrorContext {
            error_type: "timeout".into(),
            error_message: "Tool execution timed out after 30s".into(),
            phase: "tool_execution".into(),
            turn_number: Some(3),
        };
        let result = hook.on_error(&ctx).await;
        assert!(matches!(result, HookResult::Ok));
    }

    // ── ResponseValidationHook ───────────────────────────────────────

    #[test]
    fn test_response_validation_hook_trait() {
        let hook = ResponseValidationHook::new(vec!["password".into(), "secret".into()]);
        assert_eq!(hook.name(), "response-validation");
        assert!(hook.is_enabled());
    }

    #[tokio::test]
    async fn test_response_validation_no_sensitive_content() {
        let hook = ResponseValidationHook::new(vec!["password".into(), "secret".into()]);
        let turn_ctx = TurnStartContext {
            turn_number: 1,
            session_id: Some("test".into()),
            working_dir: "/tmp".into(),
            phase: "execution".into(),
            has_file_context: false,
        };
        let result = hook
            .on_model_response("Hello, how can I help you?", &turn_ctx)
            .await;
        assert!(matches!(result, HookResult::Ok));
    }

    #[tokio::test]
    async fn test_response_validation_detects_sensitive_content() {
        let hook = ResponseValidationHook::new(vec!["password".into(), "secret".into()]);
        let turn_ctx = TurnStartContext {
            turn_number: 1,
            session_id: Some("test".into()),
            working_dir: "/tmp".into(),
            phase: "execution".into(),
            has_file_context: false,
        };
        let result = hook
            .on_model_response("The admin password is super_secret_123", &turn_ctx)
            .await;
        match result {
            HookResult::Warning(msg) => {
                assert!(
                    msg.contains("password"),
                    "Warning should mention 'password'"
                );
            }
            other => panic!("Expected Warning, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_response_validation_case_insensitive() {
        let hook = ResponseValidationHook::new(vec!["SECRET".into()]);
        let turn_ctx = TurnStartContext {
            turn_number: 1,
            session_id: Some("test".into()),
            working_dir: "/tmp".into(),
            phase: "execution".into(),
            has_file_context: false,
        };
        let result = hook
            .on_model_response("This is a Secret value", &turn_ctx)
            .await;
        assert!(matches!(result, HookResult::Warning(_)));
    }
}
