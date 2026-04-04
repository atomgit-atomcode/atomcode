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
use crate::tool::result_store::ToolResultStore;
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
        let system_prompt = format!(
            "{}\n\n## SUB-AGENT CONTEXT\n\
             You are a sub-agent responsible for editing ONE file: {}\n\
             Make ALL needed changes in as few edit_file calls as possible.\n\
             Do NOT read other files — sibling skeletons are provided below.",
            rules, self.file_path,
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
        let ctx = ToolContext::new(working_dir.to_path_buf());
        let permission = Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll));

        let mut runner = TurnRunner {
            provider,
            tools,
            context: ctx,
            config: config.clone(),
            permission,
            result_store: ToolResultStore::new(ToolResultStore::default_dir()),
            recently_edited_files: Vec::new(),
            post_edit_read_counts: std::collections::HashMap::new(),
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
            let result = runner.run(&mut conversation, &system_prompt, &event_tx, cancel.clone()).await;

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
            timeout_secs: 60,
        }
    }

    /// Execute all tasks in parallel, respecting concurrency limit and timeout.
    pub async fn execute_all(
        self,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        config: &Config,
        working_dir: &std::path::Path,
    ) -> Vec<SubAgentResult> {
        use tokio::task::JoinSet;

        let timeout = Duration::from_secs(self.timeout_secs);
        let mut results: Vec<SubAgentResult> = Vec::with_capacity(self.tasks.len());

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

                set.spawn(async move {
                    tokio::time::timeout(
                        timeout,
                        task.execute(provider, tools, &config, &working_dir, 5),
                    )
                    .await
                });
            }

            while let Some(join_result) = set.join_next().await {
                match join_result {
                    Ok(Ok(result)) => results.push(result),
                    Ok(Err(_timeout)) => {
                        results.push(SubAgentResult {
                            file_path: "unknown".to_string(),
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
        assert_eq!(pool.timeout_secs, 60);
    }
}
