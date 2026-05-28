//! Sub-agent runner — orchestrates a single sub-agent execution with
//! cancellation, early exit, knowledge injection, and answer truncation.
//!
//! [`SubAgentRunner`] is the core execution engine for sub-agents. It creates
//! an isolated LLM conversation, filters parent tools per the sub-agent's
//! tool policy, runs a turn loop with cancellation support, and truncates
//! the final answer to fit within the token budget.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::config::Config;
use crate::conversation::Conversation;
use crate::hook::HookEngine;
use crate::provider::LlmProvider;
use crate::tool::{ToolContext, ToolRegistry};
use crate::turn::event::{TurnEvent, TurnResult};
use crate::turn::runner::TurnRunner;

use super::context::{build_permission_decider, build_sandbox_context};
use super::tools::build_subagent_tools;
use super::types::*;

/// Core execution engine for a single sub-agent.
///
/// Owned struct (no lifetimes — all `Arc`/owned fields) that orchestrates
/// an isolated LLM conversation with tool filtering, sandboxed context,
/// cancellation, early exit, and answer truncation.
///
/// # Flow
///
/// 1. Filter parent tools through the sub-agent's [`SubAgentToolPolicy`]
/// 2. Build a sandboxed [`ToolContext`] via `build_sandbox_context`
/// 3. Create a fresh [`Conversation`] and inject the user task
/// 4. Run a turn loop with `CancellationToken` support
/// 5. Exit early when the model responds with text and no tool calls
/// 6. Truncate the answer to `max_answer_tokens * 4` chars and return
pub struct SubAgentRunner {
    pub provider: Arc<dyn LlmProvider>,
    pub config: Arc<Config>,
    pub parent_tools: Arc<ToolRegistry>,
    pub parent_ctx: ToolContext,
    pub event_tx: mpsc::UnboundedSender<AgentEvent>,
    pub cancel_token: CancellationToken,
}

impl SubAgentRunner {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        config: Arc<Config>,
        parent_tools: Arc<ToolRegistry>,
        parent_ctx: ToolContext,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            provider,
            config,
            parent_tools,
            parent_ctx,
            event_tx,
            cancel_token,
        }
    }

    /// Execute a sub-agent defined by `def` with the given `user_task`.
    ///
    /// Returns [`SubAgentOutcome`] — either [`SubAgentOutput`] on success or
    /// [`SubAgentError`] on cancellation / LLM failure.
    pub async fn run(
        &self,
        def: SubAgentDefinition,
        user_task: String,
    ) -> SubAgentOutcome {
        tracing::info!(subagent = %def.name, task = %user_task, "sub-agent run starting");

        // ── 1. Filter tools via the sub-agent's tool policy ────────────
        let filtered_tools = Arc::new(
            build_subagent_tools(&self.parent_tools, &def.tools).await,
        );

        // ── 2. Build sandboxed ToolContext ─────────────────────────────
        let (turn_event_tx, mut turn_event_rx) = mpsc::unbounded_channel::<TurnEvent>();

        // Forward tool-level activity as concise human-readable progress.
        // Tool names are mapped to Chinese labels; no arguments or timing.
        let fwd_tx = self.event_tx.clone();
        let fwd_name = def.name.clone();
        tokio::spawn(async move {
            while let Some(event) = turn_event_rx.recv().await {
                let label = match &event {
                    TurnEvent::ToolCallStarted { name, .. } => tool_label(name),
                    _ => continue,
                };
                let _ = fwd_tx.send(AgentEvent::GuideTurnActivity {
                    subagent: fwd_name.clone(),
                    message: label.to_string(),
                });
            }
        });

        let sandbox_ctx = build_sandbox_context(
            &self.parent_ctx,
            filtered_tools.clone(),
            turn_event_tx.clone(),
        )
        .await;

        // ── 3. Create a fresh Conversation ─────────────────────────────
        let mut conversation = Conversation::new();

        // TODO: Apply compression_threshold from def for automatic
        // context compression when conversation history grows beyond
        // the threshold.  The Conversation struct currently doesn't
        // expose a direct compression field; compression is handled by
        // the CtxBuilder strategy (for_provider).  When wired, use:
        //   if conversation.estimate_tokens() > ... { ... }
        let _compression_threshold = def.compression_threshold;

        // ── 4. Inject knowledge as first user message ──────────────────
        // Placed BEFORE the user task so the model reads the reference
        // material first, then answers the question.
        if let Some(ref kb) = def.knowledge {
            let kb_text = kb.render_for_query(&user_task, def.max_knowledge_tokens);
            tracing::debug!(subagent = %def.name, kb_text_len = kb_text.len(), "knowledge injected");
            if !kb_text.is_empty() {
                conversation.add_user_message(&kb_text);
            }
        }

        // ── 5. Inject user task as a User message ──────────────────────
        conversation.add_user_message(&user_task);

        // ── 6. Build permission decider ────────────────────────────────
        // Sub-agent tools are read-only (per SubAgentToolPolicy), so
        // bypassing all approval prompts is safe.
        let permission = build_permission_decider();

        // ── 7. Build CtxBuilder from provider config ───────────────────
        let pcfg = match self.config.providers.get(&self.config.default_provider) {
            Some(pc) => pc.clone(),
            None => {
                return Err(SubAgentError {
                    turns_used: 0,
                    message: "No default provider configured".to_string(),
                    cancelled: false,
                });
            }
        };
        let build_ctx = crate::ctx::for_provider(&pcfg);

        // ── 8. Create TurnRunner with sandboxed tools & context ────────
        let mut runner = TurnRunner {
            provider: self.provider.clone(),
            tools: filtered_tools,
            context: sandbox_ctx,
            config: (*self.config).clone(),
            ctx: build_ctx,
            permission,
            hook_engine: Arc::new(HookEngine::new()),
            recently_edited_files: Vec::new(),
            loop_guard: Default::default(),
            current_turn_number: 0,
        };

        // ── 9. Turn loop with cancellation support ─────────────────────
        let mut turns_used = 0usize;
        let mut last_text = String::new();
        let max_turns = def.max_turns.max(1);

        let _ = self.event_tx.send(AgentEvent::GuideTurnActivity {
            subagent: def.name.clone(),
            message: "指南查询中...".to_string(),
        });

        // Wall-clock timeout (default 120s). The per-turn LLM calls are
        // already gated by provider-level timeouts, but the aggregate loop
        // (multiple tool-call rounds) needs its own ceiling.
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(120));
        tokio::pin!(deadline);

        for turn_idx in 0..max_turns {
            tracing::debug!(turn_idx, max_turns, "sub-agent turn");

            // Fast-path cancellation check (non-blocking)
            if self.cancel_token.is_cancelled() {
                tracing::info!(turns_used, "sub-agent cancelled");
                return Err(SubAgentError {
                    turns_used,
                    message: "Sub-agent cancelled by user".to_string(),
                    cancelled: true,
                });
            }

            // Execute one turn.  The CancellationToken is cloned and
            // passed to the runner so it can cancel mid-stream if the
            // user interrupts.
            let turn_fut = runner.run(
                &mut conversation,
                &def.system_prompt,
                &turn_event_tx,
                self.cancel_token.clone(),
            );

            // Race the turn future against the cancellation token.
            // `biased` ensures cancellation is checked first when both
            // are ready, giving priority to user interrupts.
            let result = tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    return Err(SubAgentError {
                        turns_used,
                        message: "Sub-agent cancelled during turn".to_string(),
                        cancelled: true,
                    });
                }
                _ = &mut deadline => {
                    tracing::warn!(turns_used, "sub-agent wall-clock timeout");
                    return Err(SubAgentError {
                        turns_used,
                        message: "Wall-clock timeout".to_string(),
                        cancelled: false,
                    });
                }
                result = turn_fut => result,
            };

            turns_used = turn_idx + 1;

            match result {
                TurnResult::Responded { text, .. } => {
                    // Model produced text output with no tool calls —
                    // the conversation is naturally complete.
                    last_text = text;
                    break;
                }
                TurnResult::UsedTools { text, .. } => {
                    // Model issued tool calls; results have been added
                    // to the conversation.  Continue the loop so the
                    // model can respond to the results.
                    if let Some(t) = text {
                        last_text = t;
                    }
                }
                TurnResult::Failed(err) => {
                    return Err(SubAgentError {
                        turns_used,
                        message: format!("LLM turn failed: {}", err),
                        cancelled: false,
                    });
                }
                TurnResult::Cancelled => {
                    return Err(SubAgentError {
                        turns_used,
                        message: "Sub-agent turn cancelled".to_string(),
                        cancelled: true,
                    });
                }
            }
        }

        // ── 10. Truncate answer to fit max_answer_tokens ──────────────
        // ~2 chars/token for mixed CJK+Latin content (vs ~4 for English-only).
        let max_chars = def.max_answer_tokens.saturating_mul(2);
        let truncated = last_text.chars().count() > max_chars;
        tracing::debug!(chars = last_text.chars().count(), truncated, "sub-agent answer ready");
        let text = if last_text.is_empty() {
            "\
抱歉，暂时无法回答此问题。

你可以试试：
  /guide 怎么切换模型
  /guide MCP 怎么配置
  /guide 怎么用记忆功能
  /guide 快捷键有哪些
  /guide 怎么用后台任务

也可以访问文档站：https://atomcode.atomgit.com/docs/zh/"
                .to_string()
        } else if truncated {
            // Find nearest sentence/paragraph boundary before max_chars
            // so we don't cut mid-sentence.
            let end = max_chars.min(last_text.chars().count());
            let prefix: String = last_text.chars().take(end).collect();
            let boundary = prefix
                .rfind(|c: char| c == '。' || c == '\n' || c == '.' || c == '!' || c == '?')
                .map(|p| p + 1)
                .unwrap_or(end);
            let trimmed: String = last_text.chars().take(boundary).collect();
            let trimmed = trimmed.trim_end().to_string();
            if trimmed.is_empty() {
                last_text.chars().take(end).collect()
            } else {
                trimmed
            }
        } else {
            last_text
        };

        Ok(SubAgentOutput { text, truncated })
    }
}

/// Map tool names to human-readable Chinese labels for progress display.
fn tool_label(name: &str) -> &str {
    match name {
        "read_file" => "读取文件中...",
        "grep" => "搜索代码中...",
        "glob" => "搜索文件中...",
        "list_dir" => "浏览目录中...",
        "web_search" => "搜索网页中...",
        "web_fetch" => "获取网页中...",
        _ => "处理中...",
    }
}
