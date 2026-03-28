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
}

impl TurnRunner {
    /// Execute one LLM turn: stream response, execute any tool calls, return result.
    pub async fn run(
        &self,
        conversation: &mut Conversation,
        system_prompt: &str,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> TurnResult {
        // 1. Build messages within token budget
        let context_window = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(16000);

        let (mut messages, _ctx_stats) =
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

        // 3. Get tool definitions for the LLM
        let tool_defs = self.tools.get_definitions();

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

        loop {
            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    conversation.finalize_stream();
                    return TurnResult::Cancelled;
                }

                event = stream.next() => {
                    match event {
                        Some(Ok(StreamEvent::Delta(text))) => {
                            // Strip model-internal tags (DeepSeek <think>, QwQ, etc.)
                            let text = strip_model_tags(&text);
                            if !text.is_empty() {
                                conversation.push_delta(&text);
                                text_buf.push_str(&text);
                                let _ = event_tx.send(TurnEvent::TextDelta(text));
                            }
                        }

                        Some(Ok(StreamEvent::ToolCallStart { id, name })) => {
                            conversation.tool_call_buffer = Some(ToolCallBuffer {
                                id,
                                name,
                                arguments: String::new(),
                            });
                        }

                        Some(Ok(StreamEvent::ToolCallDelta(args))) => {
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

        // 6. Execute tool calls
        let tool_count = tool_calls_buf.len();
        for call in &tool_calls_buf {
            if cancel.is_cancelled() {
                return TurnResult::Cancelled;
            }
            let result = self.execute_single_tool(call, event_tx).await;
            conversation.add_tool_result(result);
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
