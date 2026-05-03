//! Sub-agent parallel execution for multi-file tasks.
//!
//! Each SubAgent handles one file with its own Conversation + TurnRunner,
//! running in parallel via tokio::JoinSet. This keeps each sub-agent's
//! context small (~3-4K tokens) so weak models perform well.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::tool::{ToolContext, ToolRegistry};
use crate::turn::event::{TurnEvent, TurnResult};
use crate::turn::permission::{AutoPermissionDecider, AutoPermissionMode};
use crate::turn::runner::TurnRunner;

/// A single sub-agent task: one file to modify.
pub struct SubAgentTask {
    pub file_path: String,
    pub file_content: String,
    pub task_instruction: String,
    pub contract: String,
    pub sibling_skeletons: String,
}

/// Structured reason a sub-agent ended in failure. Replaces the old
/// `errors: Vec<String>` so callers can match on discriminant instead
/// of substring-matching on free text.
#[derive(Debug, Clone)]
pub enum SubAgentFailure {
    /// Stream-timeout-class network failure that survived one in-loop retry.
    StreamTimeoutAfterRetry,
    /// Model read the same file ≥ `hallucination_read_threshold` times
    /// without producing a successful edit, AND the recovery turn after
    /// the nudge also failed to edit.
    HallucinationLoop { reads: usize, file: String },
    /// `no_edit_runs` reached `idle_kill_threshold`. May or may not be
    /// preceded by a hallucination nudge.
    NoProgress { idle_turns: usize },
    /// Loop exited because turn budget was exhausted and zero edits had
    /// landed. (If edits landed, exit is treated as success.)
    BudgetExhaustedNoEdits,
    /// Pool wall-time wrapper (default 5min) tripped.
    SubAgentTimeout5min,
    /// Provider returned a non-timeout-class error (network, 4xx/5xx).
    ProviderError(String),
    /// `tokio::JoinSet` reported the task panicked or was cancelled at
    /// the runtime level.
    JoinError(String),
    /// User pressed Ctrl+C / cancellation token tripped.
    Cancelled,
}

/// Per-task instrumentation snapshot. Populated as `ProgressTracker`
/// observes turns; surfaced on `SubAgentResult` so the parent agent
/// (and operators reading datalog) can diagnose without re-deriving
/// from raw conversation history.
#[derive(Debug, Clone, Default)]
pub struct Diagnostic {
    pub edited_files: Vec<String>,
    pub read_counts: std::collections::HashMap<String, usize>,
    pub timeouts: usize,
    pub hallucination_nudges_sent: usize,
    pub final_budget: usize,
    pub turns_used: usize,
}

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub file_path: String,
    pub success: bool,
    pub turns_used: usize,
    pub summary: String,
    pub failures: Vec<SubAgentFailure>,
    pub diagnostic: Diagnostic,
}

/// Tool wrapper that delegates to an inner `ReadFileTool` but rejects any
/// `read_file` whose `file_path` arg differs from `assigned_file`. Used by
/// `filter_tools_for_subagent` to keep sub-agents from drifting into
/// sibling exploration.
struct ScopedReadFile {
    inner: Arc<dyn crate::tool::Tool>,
    assigned_file: String,
}

#[async_trait::async_trait]
impl crate::tool::Tool for ScopedReadFile {
    fn definition(&self) -> crate::tool::ToolDef {
        // Delegate; the LLM sees the same schema as a normal read_file.
        self.inner.definition()
    }

    fn approval(&self, args: &str) -> crate::tool::ApprovalRequirement {
        self.inner.approval(args)
    }

    fn approval_with_context(
        &self,
        args: &str,
        ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ApprovalRequirement {
        self.inner.approval_with_context(args, ctx)
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        // First: inner schema check.
        self.inner.validate_args(args)?;
        // Second: scope check. Parse args to peek at file_path.
        let parsed: serde_json::Value = serde_json::from_str(args)
            .map_err(|e| format!("scope check parse: {e}"))?;
        let path = parsed
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path != self.assigned_file {
            return Err(format!(
                "Sub-agent only reads its assigned file `{}`. \
                 Sibling content is in your prompt's skeleton section; \
                 do not call read_file for path `{}`.",
                self.assigned_file, path
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: &str,
        ctx: &crate::tool::ToolContext,
    ) -> anyhow::Result<crate::tool::ToolResult> {
        self.inner.execute(args, ctx).await
    }
}

/// Build a sub-agent ToolRegistry by selecting only whitelisted tools from
/// the parent. `read_file` is wrapped in `ScopedReadFile` so it can only
/// read `assigned_file`. All other tools (bash, web_*, change_dir, glob,
/// list_directory, write_file, grep) are absent from the result —
/// the runner's "tool not registered" path returns a structured error to
/// the model, which routes back through the LLM as a re-think signal.
///
/// Async because the parent registry's lock is `tokio::sync::RwLock`. Called
/// from `SubAgentTask::execute` (Task 8 wiring) which is itself async.
async fn filter_tools_for_subagent(
    parent: &ToolRegistry,
    assigned_file: &str,
) -> ToolRegistry {
    let filtered = ToolRegistry::new();
    for (name, tool) in parent.iter().await {
        match name.as_str() {
            "read_file" => {
                let scoped = ScopedReadFile {
                    inner: tool,
                    assigned_file: assigned_file.to_string(),
                };
                filtered
                    .register_arc("read_file".to_string(), Arc::new(scoped))
                    .await;
            }
            "edit_file" | "search_replace" => {
                filtered.register_arc(name, tool).await;
            }
            _ => {} // blacklist by omission
        }
    }
    filtered
}

impl SubAgentTask {
    /// Execute this sub-agent task with its own Conversation + TurnRunner.
    /// Runs up to `max_turns` LLM round-trips. Auto-approves all tools.
    pub async fn execute(
        &self,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        config: &Config,
        working_dir: &std::path::Path,
        max_turns: usize,
    ) -> SubAgentResult {
        // 1. Build minimal system prompt
        let rules = crate::config::prompt_sections::build_rules();
        let vue_warning = if self.file_path.ends_with(".vue") || self.file_path.ends_with(".svelte")
        {
            "\nCRITICAL: This is a Vue SFC. Edit <script> and <template> in SEPARATE edit_file calls. \
             Use old_string/new_string for each edit. Keep each edit focused on one region."
        } else {
            ""
        };

        let system_prompt = format!(
            "{}\n\n## SUB-AGENT RULES\n\
             You are a sub-agent. Your ONLY job: edit `{}`.\n\
             The file content is provided below — do NOT read_file, you already have it.\n\
             Call edit_file IMMEDIATELY on your first turn. Do NOT analyze, summarize, or plan.\n\
             Use old_string/new_string to find and replace text. One edit per call.\n\
             You are responsible for ONE file only. Ignore other files.{}",
            rules, self.file_path, vue_warning,
        );

        // 2. Create fresh Conversation with injected context
        let mut conversation = Conversation::new();
        let user_message = format!(
            "## Task\n{}\n\n## Contract\n{}\n\n## File: {}\n```\n{}\n```\n\n## Sibling files (skeleton)\n{}",
            self.task_instruction,
            self.contract,
            self.file_path,
            self.file_content,
            self.sibling_skeletons,
        );
        conversation.add_user_message(&user_message);

        // 3. Create isolated ToolContext + TurnRunner
        let tool_ctx = ToolContext::new(working_dir.to_path_buf());
        let permission = Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll));

        // Pick the same ctx strategy the parent AgentLoop would. Sub-agents
        // run on the same provider, so `for_provider` returns the matching
        // builder (DefaultCtx / OllamaCtx / future per-model strategies).
        // Falls back to a synthetic 128K-window config if the provider name
        // isn't in the config — matches AgentLoop::new's fallback.
        let build_ctx = match config.providers.get(&config.default_provider) {
            Some(pc) => crate::ctx::for_provider(pc),
            None => crate::ctx::for_provider(&crate::config::provider::ProviderConfig {
                provider_type: String::new(),
                api_key: None,
                model: String::new(),
                base_url: None,
                system_prompt: None,
                user_agent: None,
                context_window: 128_000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: true,
            }),
        };

        let hooks = crate::hook::json_config::load_hooks_config(working_dir);
        let mut runner = TurnRunner {
            provider,
            tools,
            context: tool_ctx,
            config: config.clone(),
            ctx: build_ctx,
            permission,
            recently_edited_files: Vec::new(),
            recent_calls: Vec::new(),
            file_read_counts: std::collections::HashMap::new(),
            hook_executor: std::sync::Arc::new(
                crate::hook::executor::HookExecutor::new(hooks)
            ),
        };

        // 4. Event channel (we drain but don't forward — sub-agent is silent)
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let cancel = CancellationToken::new();

        // 5. Run up to max_turns
        let mut turns_used = 0;
        let mut last_text = String::new();
        let mut failures: Vec<SubAgentFailure> = Vec::new();

        for _ in 0..max_turns {
            turns_used += 1;
            let result = runner
                .run(&mut conversation, &system_prompt, &event_tx, cancel.clone())
                .await;

            // Drain events
            while event_rx.try_recv().is_ok() {}

            match result {
                TurnResult::Responded { text, .. } => {
                    last_text = text;
                    break; // Done — model responded with text only (no more tools)
                }
                TurnResult::UsedTools { text, .. } => {
                    if let Some(t) = text {
                        last_text = t;
                    }
                    // Continue — model may need more turns
                }
                TurnResult::Failed(err) => {
                    failures.push(SubAgentFailure::ProviderError(err));
                    break;
                }
                TurnResult::Cancelled => {
                    failures.push(SubAgentFailure::Cancelled);
                    break;
                }
            }
        }

        // 6. Extract summary
        let summary = if last_text.is_empty() {
            format!("Edited {}", self.file_path)
        } else {
            // Take first 200 chars as summary
            last_text.chars().take(200).collect()
        };

        // Diagnostic stays Default::default() until Task 8 wires the
        // ProgressTracker into execute (per plan).
        SubAgentResult {
            file_path: self.file_path.clone(),
            success: failures.is_empty(),
            turns_used,
            summary,
            failures,
            diagnostic: Diagnostic::default(),
        }
    }
}

/// Pool that runs multiple SubAgentTasks in parallel with concurrency limits.
pub struct SubAgentPool {
    pub tasks: Vec<SubAgentTask>,
    pub max_concurrent: usize,
    pub timeout_secs: u64,
}

impl SubAgentPool {
    pub fn new(tasks: Vec<SubAgentTask>) -> Self {
        Self {
            tasks,
            max_concurrent: 3,
            timeout_secs: 300,
        }
    }

    /// Execute all tasks in parallel, streaming progress events.
    pub async fn execute_all(
        self,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        config: &Config,
        working_dir: &std::path::Path,
        event_tx: &tokio::sync::mpsc::UnboundedSender<super::AgentEvent>,
    ) -> Vec<SubAgentResult> {
        use tokio::task::JoinSet;

        let timeout = Duration::from_secs(self.timeout_secs);
        let total = self.tasks.len();
        let mut results: Vec<SubAgentResult> = Vec::with_capacity(total);

        // Emit header
        let _ = event_tx.send(super::AgentEvent::SubAgentProgress {
            file: String::new(),
            status: format!("Dispatching {} parallel agents...", total),
        });

        // Process in batches of max_concurrent
        let mut chunks = self.tasks.into_iter().peekable();
        while chunks.peek().is_some() {
            let batch: Vec<_> = (&mut chunks).take(self.max_concurrent).collect();
            let mut set = JoinSet::new();

            for task in batch {
                let provider = provider.clone();
                let tools = tools.clone();
                let config = config.clone();
                let working_dir = working_dir.to_path_buf();
                let tx = event_tx.clone();
                let file_name = std::path::Path::new(&task.file_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| task.file_path.clone());

                set.spawn(async move {
                    let _ = tx.send(super::AgentEvent::SubAgentProgress {
                        file: file_name.clone(),
                        status: "working...".to_string(),
                    });
                    let start = std::time::Instant::now();

                    let result = tokio::time::timeout(
                        timeout,
                        task.execute(provider, tools, &config, &working_dir, 5),
                    )
                    .await;

                    let elapsed = start.elapsed().as_secs();
                    let time_str = if elapsed >= 60 {
                        format!("{}m{}s", elapsed / 60, elapsed % 60)
                    } else {
                        format!("{}s", elapsed)
                    };
                    match &result {
                        Ok(r) => {
                            let _ = tx.send(super::AgentEvent::SubAgentProgress {
                                file: file_name.clone(),
                                status: if r.success {
                                    format!("done {} · {} turns", time_str, r.turns_used)
                                } else {
                                    format!("failed {}", time_str)
                                },
                            });
                        }
                        Err(_) => {
                            let _ = tx.send(super::AgentEvent::SubAgentProgress {
                                file: file_name.clone(),
                                status: format!("timeout {}", time_str),
                            });
                        }
                    }
                    // Return file_name alongside result for error reporting
                    (file_name, result)
                });
            }

            while let Some(join_result) = set.join_next().await {
                match join_result {
                    Ok((_, Ok(result))) => results.push(result),
                    Ok((name, Err(_timeout))) => {
                        results.push(SubAgentResult {
                            file_path: name,
                            success: false,
                            turns_used: 0,
                            summary: "Timed out".to_string(),
                            failures: vec![SubAgentFailure::SubAgentTimeout5min],
                            diagnostic: Diagnostic::default(),
                        });
                    }
                    Err(join_err) => {
                        results.push(SubAgentResult {
                            file_path: "unknown".to_string(),
                            success: false,
                            turns_used: 0,
                            summary: "Task panicked".to_string(),
                            failures: vec![SubAgentFailure::JoinError(format!("{}", join_err))],
                            diagnostic: Diagnostic::default(),
                        });
                    }
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_agent_pool_creation() {
        let pool = SubAgentPool::new(vec![
            SubAgentTask {
                file_path: "TopBar.vue".to_string(),
                file_content: "<template>...</template>".to_string(),
                task_instruction: "美化样式".to_string(),
                contract: "emit('toggleSidebar')".to_string(),
                sibling_skeletons: "App.vue: ...".to_string(),
            },
            SubAgentTask {
                file_path: "Sidebar.vue".to_string(),
                file_content: "<template>...</template>".to_string(),
                task_instruction: "美化样式".to_string(),
                contract: "props: { collapsed: Boolean }".to_string(),
                sibling_skeletons: "App.vue: ...".to_string(),
            },
        ]);
        assert_eq!(pool.tasks.len(), 2);
        assert_eq!(pool.max_concurrent, 3);
        assert_eq!(pool.timeout_secs, 300);
    }

    #[test]
    fn scoped_read_file_rejects_sibling_path() {
        use crate::tool::Tool;
        let inner = Arc::new(crate::tool::read::ReadFileTool) as Arc<dyn Tool>;
        let scoped = ScopedReadFile {
            inner,
            assigned_file: "/work/a.rs".to_string(),
        };
        let err = scoped
            .validate_args(r#"{"file_path":"/work/b.rs"}"#)
            .unwrap_err();
        assert!(
            err.contains("only reads its assigned file"),
            "expected scope rejection, got: {err}"
        );
    }

    #[test]
    fn scoped_read_file_allows_assigned_path() {
        use crate::tool::Tool;
        let inner = Arc::new(crate::tool::read::ReadFileTool) as Arc<dyn Tool>;
        let scoped = ScopedReadFile {
            inner,
            assigned_file: "/work/a.rs".to_string(),
        };
        assert!(
            scoped
                .validate_args(r#"{"file_path":"/work/a.rs"}"#)
                .is_ok(),
            "assigned file must pass"
        );
    }

    #[tokio::test]
    async fn filter_tools_for_subagent_keeps_only_whitelisted() {
        let parent = make_full_tool_registry();
        let filtered = filter_tools_for_subagent(&parent, "/work/a.rs").await;
        let names = collect_tool_names(&filtered).await;
        // Allowed:
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"search_replace".to_string()));
        assert!(names.contains(&"read_file".to_string()));
        // Blocked:
        assert!(!names.contains(&"bash".to_string()));
        assert!(!names.contains(&"web_fetch".to_string()));
        assert!(!names.contains(&"glob".to_string()));
        assert!(!names.contains(&"list_directory".to_string()));
        assert!(!names.contains(&"change_dir".to_string()));
    }

    /// Test helper: build a registry with all common tools.
    fn make_full_tool_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register_sync(Box::new(crate::tool::read::ReadFileTool));
        r.register_sync(Box::new(crate::tool::write::WriteFileTool));
        r.register_sync(Box::new(crate::tool::edit::EditFileTool));
        r.register_sync(Box::new(crate::tool::bash::BashTool));
        r.register_sync(Box::new(crate::tool::cd::CdTool));
        r.register_sync(Box::new(crate::tool::grep::GrepTool));
        r.register_sync(Box::new(crate::tool::glob::GlobTool));
        r.register_sync(Box::new(crate::tool::list_dir::ListDirTool));
        r.register_sync(Box::new(crate::tool::web_fetch::WebFetchTool));
        r.register_sync(Box::new(crate::tool::search_replace::SearchReplaceTool));
        r
    }

    /// Test helper: collect tool names from a registry via the public iter API.
    async fn collect_tool_names(r: &ToolRegistry) -> Vec<String> {
        r.iter().await.map(|(name, _)| name).collect()
    }
}
