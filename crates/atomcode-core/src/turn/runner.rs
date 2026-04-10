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
    pub provider: std::sync::Arc<dyn LlmProvider>,
    pub tools: std::sync::Arc<ToolRegistry>,
    pub context: ToolContext,
    pub config: Config,
    pub permission: Box<dyn PermissionDecider>,
    /// Files edited during the current session (tracked for context awareness).
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

        let (messages, ctx_stats) =
            conversation.to_provider_messages_budgeted(system_prompt, context_window);

        let actual_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();

        // Set budget hint for read_file dynamic threshold.
        // read_file checks this to decide full content vs skeleton.
        self.context.ctx_budget_hint.store(
            context_window.saturating_sub(actual_tokens),
            std::sync::atomic::Ordering::Relaxed,
        );
        let _ = event_tx.send(TurnEvent::ContextStats {
            system_tokens: ctx_stats.system_tokens,
            sent_tokens: actual_tokens.saturating_sub(ctx_stats.system_tokens),
            dropped_tokens: ctx_stats.dropped_tokens,
            working_set_tokens: 0,
            total_messages: messages.len(),
        });

        // 3. Get tool definitions for the LLM
        let all_tool_defs = self.tools.get_definitions();
        let mut tool_defs: Vec<_> = if let Some(filter) = allowed_tools {
            all_tool_defs.into_iter()
                .filter(|d| filter.contains(&d.name))
                .collect()
        } else {
            all_tool_defs
        };

        // Inject ALL known-existing files into write_file description.
        // Includes both edited AND read files — anything the model touched exists on disk.
        {
            let mut known_files: Vec<String> = self.recently_edited_files.clone();
            // Extract read files from conversation tool calls
            for msg in &messages {
                if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                    for call in tool_calls {
                        if call.name == "read_file" {
                            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                                    let short = fp.rsplit('/').next().unwrap_or(fp).to_string();
                                    if !known_files.contains(&short) {
                                        known_files.push(short);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if !known_files.is_empty() {
                if let Some(wf) = tool_defs.iter_mut().find(|d| d.name == "create_file") {
                    // Display basenames for readability in tool description
                    let display_names: Vec<&str> = known_files.iter()
                        .map(|p| p.rsplit('/').next().unwrap_or(p.as_str()))
                        .collect();
                    let list = if display_names.len() <= 6 {
                        display_names.join(", ")
                    } else {
                        format!("{}, ... ({} files)", display_names[..5].join(", "), display_names.len())
                    };
                    wf.description.push_str(&format!(
                        "\nThese files ALREADY EXIST — use edit_file instead: {}",
                        list,
                    ));
                }
            }
        }

        // 3. Start streaming
        let stream_start = std::time::Instant::now();
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
        let mut was_truncated = false;

        // Stream timeouts. Defaults are 300s for both first-token and
        // subsequent-token waits, since slow domestic model providers
        // (SiliconFlow, Zhipu GLM, etc.) under thinking mode can take >3min
        // to emit a single token after a large prompt. Override via env
        // ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS / ATOMCODE_STREAM_TIMEOUT_SECS
        // for environments where you want a tighter "real hang" detector.
        fn timeout_from_env(var: &str, default_secs: u64) -> std::time::Duration {
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(std::time::Duration::from_secs)
                .unwrap_or_else(|| std::time::Duration::from_secs(default_secs))
        }
        let first_token_timeout = timeout_from_env("ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS", 300);
        let stream_timeout = timeout_from_env("ATOMCODE_STREAM_TIMEOUT_SECS", 300);

        loop {
            let timeout = if got_any_event { stream_timeout } else { first_token_timeout };
            tokio::select! {
                biased;

_ = cancel.cancelled() => {
                    conversation.finalize_stream();
                    return TurnResult::Cancelled;
                }

                _ = tokio::time::sleep(timeout) => {
                    conversation.finalize_stream();
                    return TurnResult::Failed(format!(
                        "Stream timeout: no event for {:?}",
                        timeout
                    ));
                }

                event = stream.next() => {
                    match event {
                        Some(Ok(StreamEvent::Delta(text))) => {
                            got_any_event = true;
                            // Strip model-internal tags (DeepSeek </think>`, QwQ, etc.)
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
                                cached_tokens: usage.cached_tokens,
                            });
                        }

                        Some(Ok(StreamEvent::Done { truncated: is_truncated })) => {
                            // Finalize conversation state
                            if !tool_calls_buf.is_empty() {
                                conversation.finalize_stream_with_tool_calls(&tool_calls_buf);
                            } else {
                                conversation.finalize_stream();
                            }
                            was_truncated = is_truncated;
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

        // Log LLM response (text + tool calls)
        let response_duration = stream_start.elapsed().as_millis() as u64;
        super::log::log_llm_response(
            &text_buf,
            &tool_calls_buf,
            self.provider.model_name(),
            0, // step is set by caller
            response_duration,
        );

        // 5. If no tool calls, we're done — LLM produced text only
        if tool_calls_buf.is_empty() {
            return TurnResult::Responded {
                text: text_buf,
                tokens: total_tokens,
                truncated: was_truncated,
            };
        }

        // 6. Auto-merge multiple edit_file calls on the same file into one multi-edit.
        // Models often generate 2+ separate edit_file calls for the same file instead of
        // using the edits array. Merging at framework level is 100% reliable vs prompt ~50%.
        merge_edit_calls(&mut tool_calls_buf);

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
            // Enforce tool filter at execution time — LLM may call tools
            // not in the provided tool_defs (e.g., during diagnosis read-only phase).
            if let Some(filter) = allowed_tools {
                if !filter.contains(&call.name.as_str()) {
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: format!(
                            "Tool '{}' is not available in this phase. Read the code first, then edit.",
                            call.name
                        ),
                        success: false,
                    };
                    let _ = event_tx.send(TurnEvent::ToolCallResult {
                        name: call.name.clone(),
                        output: result.output.clone(),
                        success: false,
                        duration: std::time::Duration::ZERO,
                    });
                    conversation.add_tool_result(result);
                    continue;
                }
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
                let result = self.execute_single_tool(call, event_tx).await;

                // Track files edited for read interception (batch + cross-turn)
                // Use full file path as key to avoid basename collisions
                // (e.g., api/__init__.py vs schemas/__init__.py).
                if matches!(call.name.as_str(), "edit_file" | "create_file") && result.success {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
                        if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                            let file_key = fp.to_string();
                            if !files_edited_this_batch.contains(&file_key) {
                                files_edited_this_batch.push(file_key.clone());
                            }
                            if !self.recently_edited_files.contains(&file_key) {
                                self.recently_edited_files.push(file_key);
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

    /// EXECUTE mode: run one LLM turn with minimal context.
    /// Reads the target file fresh from disk, sends only the file + instruction,
    /// and only exposes edit_file. Used for precise, focused edits.
    ///
    /// Returns the TurnResult and whether any file was edited.
    pub async fn run_execute(
        &mut self,
        file_path: &str,
        instruction: &str,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
        cancel: CancellationToken,
    ) -> TurnResult {
        // 1. Read fresh file content from disk
        let file_content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) => return TurnResult::Failed(format!("Cannot read {}: {}", file_path, e)),
        };

        // 2. Build minimal conversation: system + user(file + instruction)
        let system_prompt = "You are an execution agent. Your ONLY job: apply the edit instruction to the file below.\n\
            RULES:\n\
            1. Call edit_file IMMEDIATELY with old_string/new_string. Do NOT explain.\n\
            2. Do NOT read_file — the file content is already provided.\n\
            3. Do NOT fix other issues — ONLY apply the given instruction.\n\
            4. If the instruction is unclear, apply your best interpretation.";

        let user_message = format!(
            "## Instruction\n{}\n\n## File: {}\n```\n{}\n```",
            instruction, file_path, file_content,
        );

        let mut mini_conv = Conversation::new();
        mini_conv.add_user_message(&user_message);

        // 3. Only expose edit_file
        let execute_tools = &["edit_file"];

        // 4. Run the LLM turn with filtered tools
        let result = self.run_with_filter(
            &mut mini_conv,
            system_prompt,
            event_tx,
            cancel,
            Some(execute_tools),
        ).await;

        result
    }

    /// Execute a single tool call with permission checking.
    async fn execute_single_tool(
        &self,
        call: &ToolCall,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
    ) -> ToolResult {
        // Auto-fix common tool name aliases (models trained on other agents use different names)
        // Case-insensitive matching: models may output "Run", "Bash", "Edit_File", etc.
        let name_lower = call.name.to_lowercase();
        let corrected_name = match name_lower.as_str() {
            "create_file" => "write_file",
            "find" | "find_files" => "glob",
            "run" | "run_command" | "run_server" | "run_shell" | "run_app"
                | "execute" | "shell" | "terminal" => "bash",
            "list_files" | "ls" => "list_directory",
            "search" => "grep",
            _ => "",
        };
        let corrected_name = if corrected_name.is_empty() {
            // No alias match — try case-insensitive lookup in registry
            if self.tools.get(&call.name).is_some() {
                call.name.as_str()
            } else if let Some(name) = self.tools.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&call.name))
                .map(|(k, _)| k.as_ref())
            {
                name
            } else {
                call.name.as_str()
            }
        } else {
            corrected_name
        };
        let tool = match self.tools.get(corrected_name) {
            Some(t) => t,
            None => {
                let available: String = self.tools.iter()
                    .map(|(name, _)| name.as_ref())
                    .collect::<Vec<&str>>()
                    .join(", ");
                let hint = match call.name.as_str() {
                    "create_file" => "\nDid you mean write_file? create_file was renamed to write_file.",
                    "search" => "\nFor file content search: grep(pattern, path)\nFor web search: web_search(query)",
                    _ => "",
                };
                let output = format!(
                    "Error: unknown tool '{}'. Available tools: {}.{}",
                    call.name, available, hint
                );
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

        // Use corrected name for all subsequent checks
        let call = if corrected_name != call.name.as_str() {
            &ToolCall {
                id: call.id.clone(),
                name: corrected_name.to_string(),
                arguments: call.arguments.clone(),
            }
        } else {
            call
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

/// Merge multiple edit_file calls targeting the same file into a single multi-edit call.
/// The model often generates 2+ separate edit_file(file, old, new) for the same file;
/// we merge them into one edit_file(file, edits=[...]) before execution.
fn merge_edit_calls(calls: &mut Vec<ToolCall>) {
    use std::collections::HashMap;

    // Group edit_file calls by file_path. Preserve order of first occurrence.
    let mut file_groups: HashMap<String, Vec<usize>> = HashMap::new();
    let mut file_order: Vec<String> = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        if call.name != "edit_file" { continue; }
        let fp = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()
            .and_then(|a| a.get("file_path").and_then(|v| v.as_str()).map(String::from));
        if let Some(fp) = fp {
            let entry = file_groups.entry(fp.clone()).or_default();
            if entry.is_empty() { file_order.push(fp); }
            entry.push(i);
        }
    }

    // Only merge groups with 2+ calls
    let merge_targets: Vec<(String, Vec<usize>)> = file_order.into_iter()
        .filter_map(|fp| {
            let indices = file_groups.remove(&fp)?;
            if indices.len() >= 2 { Some((fp, indices)) } else { None }
        })
        .collect();

    if merge_targets.is_empty() { return; }

    let mut remove_indices: Vec<usize> = Vec::new();
    for (file_path, indices) in &merge_targets {
        // Build edits array from individual calls
        let mut edits: Vec<serde_json::Value> = Vec::new();
        for &idx in indices {
            let args: serde_json::Value = serde_json::from_str(&calls[idx].arguments)
                .unwrap_or_default();
            let mut edit = serde_json::Map::new();
            if let Some(v) = args.get("old_string") { edit.insert("old_string".into(), v.clone()); }
            if let Some(v) = args.get("new_string") { edit.insert("new_string".into(), v.clone()); }
            if let Some(v) = args.get("start_line") { edit.insert("start_line".into(), v.clone()); }
            if let Some(v) = args.get("end_line") { edit.insert("end_line".into(), v.clone()); }
            edits.push(serde_json::Value::Object(edit));
        }

        // Replace first call with merged version, mark rest for removal
        let first_idx = indices[0];
        let merged_args = serde_json::json!({
            "file_path": file_path,
            "edits": edits,
        });
        calls[first_idx].arguments = merged_args.to_string();
        remove_indices.extend(&indices[1..]);
    }

    // Remove merged calls (reverse order to preserve indices)
    remove_indices.sort_unstable();
    remove_indices.dedup();
    for idx in remove_indices.into_iter().rev() {
        calls.remove(idx);
    }
}
