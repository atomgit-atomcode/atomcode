use std::time::Instant;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::stream::StreamEvent;
use crate::tool::{
    PermissionDecision, ToolCall, ToolCallBuffer, ToolContext, ToolRegistry, ToolResult,
};

use super::event::{TurnEvent, TurnResult};
use super::permission::PermissionDecider;

/// Core LLM streaming + tool execution primitive.
///
/// Handles exactly one LLM call cycle:
/// 1. Build messages from conversation
/// 2. Stream LLM response (text deltas + tool calls)
/// 3. Execute tool calls (with permission checking)
/// 4. Add results to conversation
///
/// Does NOT handle: retries, discipline (anti-loop, step limits), or conversation management.
/// The caller (AgentLoop / SubagentLoop) owns those responsibilities.
pub struct TurnRunner {
    pub provider: Box<dyn LlmProvider>,
    pub tools: std::sync::Arc<ToolRegistry>,
    pub context: ToolContext,
    pub config: Config,
    pub permission: Box<dyn PermissionDecider>,
    /// Tool result store — used to inflate ToolResultRef messages before sending to LLM.
    pub result_store: crate::tool::result_store::ToolResultStore,
    /// Files edited during the current session — read_file on these is intercepted
    /// to prevent wasteful "verification reads" after editing.
    pub recently_edited_files: Vec<String>,
}

impl TurnRunner {
    /// Execute one LLM turn: stream response, execute any tool calls, return result.
    pub async fn run(
        &mut self,
        conversation: &mut Conversation,
        system_prompt: &str,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> TurnResult {
        self.run_with_filter(conversation, system_prompt, event_tx, cancel, None).await
    }

    /// Run with optional tool filter. If `allowed_tools` is Some, only those tools
    /// are visible to the LLM. Used by Phase 2 to restrict first turn to read-only.
    pub async fn run_with_filter(
        &mut self,
        conversation: &mut Conversation,
        system_prompt: &str,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
        allowed_tools: Option<&[&str]>,
    ) -> TurnResult {
        // 1. Build messages within token budget
        let context_window = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(16000);

        let (mut messages, ctx_stats) =
            conversation.to_provider_messages_budgeted(system_prompt, context_window);

        // 2. Inflate ToolResultRef → ToolResult for recent messages.
        // Conversation stores large results as compact refs on disk;
        // inflate the last 20 so the LLM sees actual tool output.
        {
            let mut inflated = 0usize;
            for msg in messages.iter_mut().rev() {
                if inflated >= 20 { break; }
                if let crate::conversation::message::MessageContent::ToolResultRef(ref r) = msg.content {
                    let full = self.result_store.inflate(r);
                    msg.content = crate::conversation::message::MessageContent::ToolResult(full);
                    inflated += 1;
                }
            }
        }

        // Emit context stats AFTER inflate so datalog reflects actual tokens sent to LLM.
        // Pre-inflate stats were misleading (ToolResultRef counted as ~50 tokens,
        // but inflate expands them to 5K-20K).
        let actual_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();
        let _ = event_tx.send(TurnEvent::ContextStats {
            system_tokens: ctx_stats.system_tokens,
            hot_tokens: actual_tokens.saturating_sub(ctx_stats.system_tokens),
            cold_tokens: ctx_stats.cold_tokens,
            working_set_tokens: 0,
            total_messages: messages.len(),
        });

        // 3. Get tool definitions for the LLM
        let all_tool_defs = self.tools.get_definitions();
        let tool_defs: Vec<_> = if let Some(filter) = allowed_tools {
            all_tool_defs.into_iter()
                .filter(|d| filter.contains(&d.name))
                .collect()
        } else {
            all_tool_defs
        };

        // 3. Start streaming
        let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => return TurnResult::Failed(e.to_string()),
        };

        // 4. Process stream events
        let mut tool_calls_buf: Vec<ToolCall> = Vec::new();
        let mut text_buf = String::new();
        let mut total_tokens: usize = 0;
        let mut got_any_event = false;

        // Timeout: 180s for first token, 180s for subsequent tokens.
        // Domestic model providers (SiliconFlow, etc.) can be very slow under load.
        const FIRST_TOKEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
        const STREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

        loop {
            let timeout = if got_any_event { STREAM_TIMEOUT } else { FIRST_TOKEN_TIMEOUT };
            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    conversation.finalize_stream();
                    return TurnResult::Cancelled;
                }

                _ = tokio::time::sleep(timeout) => {
                    conversation.finalize_stream();
                    let waited = if got_any_event { "3 min" } else { "5 min" };
                    return TurnResult::Failed(format!(
                        "LLM stream timeout: no response for {}. Connection may have dropped.",
                        waited,
                    ));
                }

                event = stream.next() => {
                    match event {
                        Some(Ok(StreamEvent::Delta(text))) => {
                            got_any_event = true;
                            // Strip model-internal tags (DeepSeek <think>, QwQ, etc.)
                            let text = strip_model_tags(&text);
                            if !text.is_empty() {
                                conversation.push_delta(&text);
                                text_buf.push_str(&text);
                                let _ = event_tx.send(TurnEvent::TextDelta(text));
                            }
                        }

                        Some(Ok(StreamEvent::ToolCallStart { id, name })) => {
                            got_any_event = true;
                            conversation.tool_call_buffer = Some(ToolCallBuffer {
                                id,
                                name,
                                arguments: String::new(),
                            });
                        }

                        Some(Ok(StreamEvent::ToolCallDelta(args))) => {
                            got_any_event = true;
                            if let Some(ref mut buf) = conversation.tool_call_buffer {
                                buf.arguments.push_str(&args);
                            }
                        }

                        Some(Ok(StreamEvent::ToolCallDone(call))) => {
                            conversation.tool_call_buffer = None;
                            let _ = event_tx.send(TurnEvent::ToolCallStarted {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            });
                            tool_calls_buf.push(call);
                        }

                        Some(Ok(StreamEvent::Usage(usage))) => {
                            total_tokens += usage.completion_tokens;
                            let _ = event_tx.send(TurnEvent::TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                total_tokens: usage.prompt_tokens + usage.completion_tokens,
                            });
                        }

                        Some(Ok(StreamEvent::Done)) => {
                            // Finalize conversation state
                            if !tool_calls_buf.is_empty() {
                                conversation.finalize_stream_with_tool_calls(&tool_calls_buf);
                            } else {
                                conversation.finalize_stream();
                            }
                            break;
                        }

                        Some(Ok(StreamEvent::Error(e))) => {
                            conversation.finalize_stream();
                            return TurnResult::Failed(e);
                        }

                        Some(Err(e)) => {
                            conversation.finalize_stream();
                            return TurnResult::Failed(e.to_string());
                        }

                        None => {
                            // Stream ended without Done event
                            conversation.finalize_stream();
                            break;
                        }
                    }
                }
            }
        }

        // 5. If no tool calls, we're done — LLM produced text only
        if tool_calls_buf.is_empty() {
            return TurnResult::Responded {
                text: text_buf,
                tokens: total_tokens,
            };
        }

        // 6. Execute tool calls (with dedup for identical calls in the same batch)
        let tool_count = tool_calls_buf.len();
        let mut seen_calls: std::collections::HashMap<(String, String), usize> = std::collections::HashMap::new();
        let mut is_dup: Vec<bool> = vec![false; tool_calls_buf.len()];
        for (i, call) in tool_calls_buf.iter().enumerate() {
            let key = (call.name.clone(), call.arguments.clone());
            if seen_calls.contains_key(&key) {
                is_dup[i] = true;
            } else {
                seen_calls.insert(key, i);
            }
        }
        let mut files_edited_this_batch: Vec<String> = Vec::new();
        for (i, call) in tool_calls_buf.iter().enumerate() {
            if cancel.is_cancelled() {
                return TurnResult::Cancelled;
            }
            if is_dup[i] {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: "[Duplicate call — same tool and arguments as an earlier call in this batch. \
                             Result already returned above.]".to_string(),
                    success: true,
                };
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    name: call.name.clone(),
                    output: result.output.clone(),
                    success: true,
                    duration: std::time::Duration::ZERO,
                });
                conversation.add_tool_result(result);
            } else {
                // Intercept: block read_file on a file that was recently edited.
                // Prevents the model from wasting turns "verifying" its own edits.
                // Checks both this batch AND across turns (recently_edited_files).
                if call.name == "read_file" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                            let short = std::path::Path::new(fp)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| fp.to_string());
                            let edited_recently = files_edited_this_batch.iter()
                                .chain(self.recently_edited_files.iter())
                                .any(|f| f == &short || fp.contains(f.as_str()));
                            if edited_recently {
                                let result = ToolResult {
                                    call_id: call.id.clone(),
                                    output: format!(
                                        "[SKIPPED: {} was just edited. The edit result above shows the current state. \
                                         Do NOT re-read files you just edited. Proceed to the next task or summarize.]",
                                        short
                                    ),
                                    success: true,
                                };
                                let _ = event_tx.send(TurnEvent::ToolCallResult {
                                    name: call.name.clone(),
                                    output: result.output.clone(),
                                    success: true,
                                    duration: std::time::Duration::ZERO,
                                });
                                conversation.add_tool_result(result);
                                continue;
                            }
                        }
                    }
                }

                let result = self.execute_single_tool(call, event_tx).await;

                // Track files edited for read interception (batch + cross-turn)
                if matches!(call.name.as_str(), "edit_file" | "write_file") && result.success {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                            let short = std::path::Path::new(fp)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| fp.to_string());
                            if !files_edited_this_batch.contains(&short) {
                                files_edited_this_batch.push(short.clone());
                            }
                            if !self.recently_edited_files.contains(&short) {
                                self.recently_edited_files.push(short);
                            }
                        }
                    }
                }

                conversation.add_tool_result(result);
            }
        }

        TurnResult::UsedTools {
            text: if text_buf.is_empty() {
                None
            } else {
                Some(text_buf)
            },
            tool_count,
            tokens: total_tokens,
        }
    }

    /// Execute a single tool call with permission checking.
    async fn execute_single_tool(
        &self,
        call: &ToolCall,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
    ) -> ToolResult {
        let tool = match self.tools.get(&call.name) {
            Some(t) => t,
            None => {
                let output = format!("Error: unknown tool '{}'", call.name);
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    name: call.name.clone(),
                    output: output.clone(),
                    success: false,
                    duration: std::time::Duration::ZERO,
                });
                return ToolResult {
                    call_id: call.id.clone(),
                    output,
                    success: false,
                };
            }
        };

        // Check permission via the injected PermissionDecider.
        // AutoApprove tools execute immediately; RequireApproval tools go through
        // the decider which handles interactive prompts or automatic policy.
        let approval = tool.approval(&call.arguments);
        if let crate::tool::ApprovalRequirement::RequireApproval(ref reason) = approval {
            let decision = self.permission.decide(call, reason).await;
            if !matches!(decision, PermissionDecision::Allow) {
                let output = format!("Tool '{}' was denied by the user.", call.name);
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    name: call.name.clone(),
                    output: output.clone(),
                    success: false,
                    duration: std::time::Duration::ZERO,
                });
                return ToolResult {
                    call_id: call.id.clone(),
                    output,
                    success: false,
                };
            }
        }

        // Execute the tool
        let start = Instant::now();
        let result = tool.execute(&call.arguments, &self.context).await;
        let duration = start.elapsed();

        let tool_result = match result {
            Ok(mut r) => {
                r.call_id = call.id.clone();
                r
            }
            Err(e) => ToolResult {
                call_id: call.id.clone(),
                output: format!("Error: {}", e),
                success: false,
            },
        };

        let _ = event_tx.send(TurnEvent::ToolCallResult {
            name: call.name.clone(),
            output: tool_result.output.clone(),
            success: tool_result.success,
            duration,
        });

        tool_result
    }
}

/// Strip model-internal reasoning tags from streaming output.
/// DeepSeek uses `<think>...</think>`, QwQ uses similar patterns.
/// These should not be shown to the user or stored in conversation.
fn strip_model_tags(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<think>") {
        if let Some(end) = result.find("</think>") {
            let end = end + "</think>".len();
            result = format!("{}{}", &result[..start], &result[end..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    result = result.replace("</think>", "");
    result = result.replace("<|im_start|>", "").replace("<|im_end|>", "");
    result
}
