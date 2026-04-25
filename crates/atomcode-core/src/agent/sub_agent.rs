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

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub file_path: String,
    pub success: bool,
    pub turns_used: usize,
    pub summary: String,
    pub errors: Vec<String>,
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
                ephemeral: true,
            }),
        };

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
        };

        // 4. Event channel (we drain but don't forward — sub-agent is silent)
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let cancel = CancellationToken::new();

        // 5. Run up to max_turns
        let mut turns_used = 0;
        let mut last_text = String::new();
        let mut errors = Vec::new();

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
                    errors.push(err);
                    break;
                }
                TurnResult::Cancelled => {
                    errors.push("Cancelled".to_string());
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

        SubAgentResult {
            file_path: self.file_path.clone(),
            success: errors.is_empty(),
            turns_used,
            summary,
            errors,
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
                            errors: vec!["Sub-agent timed out".to_string()],
                        });
                    }
                    Err(join_err) => {
                        results.push(SubAgentResult {
                            file_path: "unknown".to_string(),
                            success: false,
                            turns_used: 0,
                            summary: "Task panicked".to_string(),
                            errors: vec![format!("JoinError: {}", join_err)],
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
}
