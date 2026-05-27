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
        // ── 1. Filter tools via the sub-agent's tool policy ────────────
        let filtered_tools = Arc::new(
            build_subagent_tools(&self.parent_tools, &def.tools).await,
        );

        // ── 2. Build sandboxed ToolContext ─────────────────────────────
        // Create a TurnEvent channel for tool output streaming. The
        // receiver is dropped — we don't forward sub-agent TurnEvents
        // to the parent's event loop.
        let (turn_event_tx, _turn_event_rx) = mpsc::unbounded_channel::<TurnEvent>();
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

        // ── 4. Inject knowledge base (placeholder) ─────────────────────
        // TODO: When `KnowledgeRef` is replaced with a concrete type,
        // inject knowledge as a user message here.  The `knowledge`
        // field is currently `Option<()>` so there is nothing to inject.
        if def.knowledge.is_some() {
            // Knowledge injection is not yet wired; see Task 10.
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

        for turn_idx in 0..max_turns {
            // Send progress notification to the parent agent
            let _ = self.event_tx.send(AgentEvent::GuideTurnActivity {
                subagent: def.name.clone(),
                message: format!("turn {}/{}", turn_idx + 1, max_turns),
            });

            // Fast-path cancellation check (non-blocking)
            if self.cancel_token.is_cancelled() {
                return Err(SubAgentError {
                    turns_used,
                    message: "Sub-agent cancelled by user".to_string(),
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
                    });
                }
                TurnResult::Cancelled => {
                    return Err(SubAgentError {
                        turns_used,
                        message: "Sub-agent turn cancelled".to_string(),
                    });
                }
            }
        }

        // ── 10. Truncate answer to fit max_answer_tokens ──────────────
        // Estimate: 1 token ≈ 4 characters of average text.  This is a
        // coarse upper-bound; actual tokenisation may vary by model.
        let max_chars = def.max_answer_tokens.saturating_mul(4);
        let truncated = last_text.chars().count() > max_chars;
        let text = if truncated {
            last_text.chars().take(max_chars).collect()
        } else {
            last_text
        };

        Ok(SubAgentOutput { text, truncated })
    }
}
