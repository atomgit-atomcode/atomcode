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
    /// Rolling history of `(tool_name, args_hash)` pairs — used to detect tool
    /// call loops (same tool + same args repeated without any edit in between).
    /// Bounded to 20 entries to keep memory flat. For `read_file`, only the
    /// file_path is hashed so paginated re-reads of the same file are treated
    /// as repeats.
    pub recent_calls: Vec<(String, u64)>,
    /// Per-basename read counter. 5+ consecutive reads of the same file without
    /// an edit is an infinite loop (common when a file can't be parsed as text
    /// — e.g. Office binaries — and the model keeps retrying with different
    /// offset/limit values instead of giving up).
    pub file_read_counts: std::collections::HashMap<String, u32>,
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
        self.run_with_filter(conversation, system_prompt, "", event_tx, cancel, None).await
    }

    /// Run with optional tool filter and turn reminder.
    /// `turn_reminder` is dynamic per-turn context (git status, current task, etc.)
    /// injected as a <system-reminder> into the last user message to keep the
    /// system prompt stable for caching.
    pub async fn run_with_filter(
        &mut self,
        conversation: &mut Conversation,
        system_prompt: &str,
        turn_reminder: &str,
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
            .unwrap_or(128000);

        let (mut messages, ctx_stats) =
            conversation.to_provider_messages_budgeted(system_prompt, context_window);

        // Inject turn reminder into the last user message.
        // This keeps system prompt stable (cacheable) while providing
        // per-turn dynamic context (previous session, current task, etc.).
        if !turn_reminder.is_empty() {
            // Find the last User message and prepend the reminder
            for msg in messages.iter_mut().rev() {
                if matches!(msg.role, crate::conversation::message::Role::User) {
                    if let crate::conversation::message::MessageContent::Text(ref mut text) = msg.content {
                        *text = format!("{}\n{}", turn_reminder, text);
                        break;
                    }
                }
            }
        }

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
        // Reasoning-model thinking content collected separately — not emitted
        // to scrollback by default (users don't want to read the thinking).
        // If `text_buf` ends up empty at `Done` but this is non-empty, we
        // promote reasoning to the final answer: some gateways route entire
        // responses through `reasoning_content` for MiniMax-M2.7 / DeepSeek-R1,
        // and without the fallback we'd return a silent 0-token "Nailed it".
        let mut reasoning_buf = String::new();
        let mut total_tokens: usize = 0;
        let mut got_usage = false;
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
                        Some(Ok(StreamEvent::Reasoning(text))) => {
                            got_any_event = true;
                            // Accumulate only. Don't push into conversation / emit
                            // TextDelta here — default UX is to hide reasoning.
                            // If `content` ends up empty, the `Done` arm below
                            // promotes `reasoning_buf` to the answer.
                            reasoning_buf.push_str(&text);
                        }
                        Some(Ok(StreamEvent::ToolCallStart { id, name })) => {
                            got_any_event = true;
                            // Surface the tool name to UI immediately — otherwise users see
                            // "Generating…" for the entire args-streaming window (can be 30s+
                            // for large write_file calls).
                            let _ = event_tx.send(TurnEvent::ToolCallStreaming { name: name.clone(), hint: String::new() });
                            conversation.tool_call_buffer = Some(ToolCallBuffer {
                                id,
                                name,
                                arguments: String::new(),
                                hint_sent: false,
                            });
                        }

                        Some(Ok(StreamEvent::ToolCallDelta(args))) => {
                            got_any_event = true;
                            if let Some(ref mut buf) = conversation.tool_call_buffer {
                                buf.arguments.push_str(&args);
                                // Extract file_path from partial args (once only).
                                if !buf.hint_sent && buf.arguments.len() < 300 {
                                    if let Some(hint) = extract_path_hint(&buf.arguments) {
                                        buf.hint_sent = true;
                                        let _ = event_tx.send(TurnEvent::ToolCallStreaming {
                                            name: buf.name.clone(),
                                            hint,
                                        });
                                    }
                                }
                            }
                        }

                        Some(Ok(StreamEvent::ToolCallDone(call))) => {
                            conversation.tool_call_buffer = None;
                            let _ = event_tx.send(TurnEvent::ToolCallStarted {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            });
                            tool_calls_buf.push(call);
                        }

                        Some(Ok(StreamEvent::Usage(usage))) => {
                            total_tokens += usage.completion_tokens;
                            got_usage = true;
                            let _ = event_tx.send(TurnEvent::TokenUsage {
                                prompt_tokens: usage.prompt_tokens,
                                completion_tokens: usage.completion_tokens,
                                total_tokens: usage.prompt_tokens + usage.completion_tokens,
                                cached_tokens: usage.cached_tokens,
                            });
                        }

                        Some(Ok(StreamEvent::Done { truncated: is_truncated })) => {
                            // Reasoning-only fallback: some gateways route the
                            // entire response through `reasoning_content` for
                            // reasoning models (MiniMax-M2.7, DeepSeek-R1). If
                            // we end up here with empty `content`, empty
                            // tool_calls, but a non-empty reasoning buffer, treat
                            // the reasoning as the answer — otherwise the agent's
                            // empty-response retry loop fires twice, sleeps 4s,
                            // and finally reports a silent "Nailed it · 0 tok".
                            //
                            // Rescue runs before this so real tool-call-in-text
                            // escapes still take priority.
                            let rescued_tools = if tool_calls_buf.is_empty() {
                                let rescued = rescue_text_tool_calls(&text_buf);
                                if !rescued.is_empty() {
                                    conversation.clear_stream_buffer();
                                    tool_calls_buf.extend(rescued);
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };

                            if text_buf.trim().is_empty()
                                && tool_calls_buf.is_empty()
                                && !rescued_tools
                                && !reasoning_buf.trim().is_empty()
                            {
                                let promoted = std::mem::take(&mut reasoning_buf);
                                conversation.push_delta(&promoted);
                                text_buf.push_str(&promoted);
                                let _ = event_tx.send(TurnEvent::TextDelta(promoted));
                            }

                            // Fallback: if the provider didn't report usage (many
                            // OpenAI-compatible APIs ignore stream_options), estimate
                            // output tokens from the streamed text + tool call args.
                            if !got_usage {
                                let mut output_chars = text_buf.len();
                                for tc in &tool_calls_buf {
                                    output_chars += tc.arguments.len();
                                }
                                // Rough heuristic: ~2 chars per token for mixed
                                // Chinese/English, ~4 for pure English. Use 3 as a
                                // middle ground since most users mix both.
                                let estimated = (output_chars / 3).max(1);
                                total_tokens += estimated;
                                let _ = event_tx.send(TurnEvent::TokenUsage {
                                    prompt_tokens: 0,
                                    completion_tokens: estimated,
                                    total_tokens: estimated,
                                    cached_tokens: 0,
                                });
                            }

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

        // Log LLM response (text + tool calls) to <working_dir>/datalog/llm/
        let response_duration = stream_start.elapsed().as_millis() as u64;
        let wd = self.context.working_dir
            .try_read().map(|g| g.clone()).unwrap_or_default();
        super::log::log_llm_response(
            &wd,
            &text_buf,
            &tool_calls_buf,
            self.provider.model_name(),
            0, // step is set by caller
            response_duration,
            self.config.datalog.enabled,
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
        //
        // Each merged-away call had its own ToolCallStarted emitted upstream — we MUST
        // emit a matching ToolCallResult so the TUI's in-flight spinner stops animating
        // for those orphan ids.
        let merged_away_ids = merge_edit_calls(&mut tool_calls_buf);
        for merged_id in &merged_away_ids {
            let _ = event_tx.send(TurnEvent::ToolCallResult {
                call_id: merged_id.clone(),
                name: "edit_file".to_string(),
                output: "[merged into adjacent edit_file call on same file]".to_string(),
                success: true,
                duration: std::time::Duration::ZERO,
            });
        }

        // ── Layer B: per-turn read budget allocation ──
        // Count read_file calls in this batch and set per-file token budget.
        // Formula: 20% of ctx budget / num_reads. This ensures N reads in one
        // turn share the budget fairly — 1 read gets 20%, 3 reads get 6.7% each.
        // read.rs Layer A checks file_tokens against this to decide full vs skeleton.
        {
            let num_reads = tool_calls_buf.iter()
                .filter(|c| c.name == "read_file")
                .count()
                .max(1); // avoid division by zero
            let budget = self.context.ctx_budget_hint.load(std::sync::atomic::Ordering::Relaxed);
            let per_file = budget / (5 * num_reads);
            self.context.read_budget_tokens.store(
                per_file.max(2000), // floor: ~170 lines always get full content
                std::sync::atomic::Ordering::Relaxed,
            );
        }

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
                        call_id: call.id.clone(),
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
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: result.output.clone(),
                    success: true,
                    duration: std::time::Duration::ZERO,
                });
                conversation.add_tool_result(result);
            } else {
                let result = self.execute_single_tool(call, event_tx, &cancel).await;

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
            "",
            event_tx,
            cancel,
            Some(execute_tools),
        ).await;

        result
    }

    /// Execute a single tool call with permission checking.
    ///
    /// `cancel` is polled while the tool future runs so Ctrl+C interrupts
    /// mid-execution — without this, long-running tools (deep `glob`, slow
    /// `grep`, network calls) complete before the turn-level cancel check
    /// runs on the next iteration, and the user sees an unresponsive UI.
    async fn execute_single_tool(
        &mut self,
        call: &ToolCall,
        event_tx: &mpsc::UnboundedSender<TurnEvent>,
        cancel: &CancellationToken,
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
        // Clone the Arc so the borrow of `self.tools` ends here — we need to
        // call `self.detect_call_loop(..)` mutably below.
        let tool = match self.tools.get_arc(corrected_name) {
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
                    call_id: call.id.clone(),
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

        // Repair malformed JSON args before approval and execution.
        // Providers sometimes emit truncated / unescaped / fenced JSON (especially
        // on max_tokens cutoff mid-arguments). Running the repair chain here means
        // tool implementations see valid JSON whenever we can salvage anything,
        // and surface deterministic errors when we can't.
        let repaired_args = super::json_repair::repair_tool_args(corrected_name, &call.arguments);

        // Use corrected name and repaired args for all subsequent checks
        let owned_call;
        let call = if corrected_name != call.name.as_str() || repaired_args != call.arguments {
            owned_call = ToolCall {
                id: call.id.clone(),
                name: corrected_name.to_string(),
                arguments: repaired_args,
            };
            &owned_call
        } else {
            call
        };

        // Loop detection: block before we even ask for approval. Without this,
        // models that get stuck (e.g. re-reading a binary Office file with
        // different offset/limit values) can burn 30+ turns on the same call.
        // Returns a user-facing message when blocked; the tool never runs.
        if let Some(msg) = self.detect_call_loop(&call.name, &call.arguments) {
            let _ = event_tx.send(TurnEvent::ToolCallResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: msg.clone(),
                success: false,
                duration: std::time::Duration::ZERO,
            });
            return ToolResult {
                call_id: call.id.clone(),
                output: msg,
                success: false,
            };
        }

        // Check permission via the injected PermissionDecider.
        // AutoApprove tools execute immediately; RequireApproval tools go through
        // the decider which handles interactive prompts or automatic policy.
        let approval = tool.approval(&call.arguments);
        if let crate::tool::ApprovalRequirement::RequireApproval(ref reason) = approval {
            let decision = self.permission.decide(call, reason).await;
            if !matches!(decision, PermissionDecision::Allow) {
                let output = format!("Tool '{}' was denied by the user.", call.name);
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    call_id: call.id.clone(),
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

        // Execute the tool. Race against `cancel` so Ctrl+C aborts a
        // long-running tool future instead of waiting for it to finish.
        // Dropping the tool future is safe for read-only tools (glob /
        // grep / read_file); mutating tools (write_file / edit_file /
        // bash) finish fast enough that interrupting them mid-execution
        // is acceptable — user pressed Ctrl+C knowing they want to stop.
        let start = Instant::now();
        let result = tokio::select! {
            r = tool.execute(&call.arguments, &self.context) => r,
            _ = cancel.cancelled() => {
                let duration = start.elapsed();
                let output = "[Cancelled by user]".to_string();
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                    success: false,
                    duration,
                });
                return ToolResult {
                    call_id: call.id.clone(),
                    output,
                    success: false,
                };
            }
        };
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
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: tool_result.output.clone(),
            success: tool_result.success,
            duration,
        });

        tool_result
    }

    /// Detect tool-call loops and return a recovery message when one should be
    /// blocked. Also updates the rolling call history as a side effect.
    ///
    /// Two patterns are caught:
    ///
    /// 1. **Paginated re-reads of the same file** (`read_file` specific):
    ///    5 unbroken `read_file` calls against the same basename — typically
    ///    the model panicking on an unreadable file (Office binary, missing
    ///    GBK decode, etc.) and cycling through offset/limit combinations.
    ///    Reset by a successful `edit_file` / `write_file` targeting the
    ///    same file (that counts as real progress).
    ///
    /// 2. **Exact repeats** (any tool): 3 calls with identical `(tool_name,
    ///    args_hash)` and no intervening `edit_file` / `write_file`. Means
    ///    the model re-issued the same command without reacting to the
    ///    previous failure.
    ///
    /// For `read_file`, only `file_path` is hashed so `offset`/`limit`
    /// variations still count as "same call".
    fn detect_call_loop(&mut self, tool_name: &str, args: &str) -> Option<String> {
        use std::hash::{Hash, Hasher};

        // --- Pattern 1: per-file read saturation ------------------------------
        if tool_name == "read_file" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                    let short = std::path::Path::new(fp)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fp.to_string());
                    let count = self.file_read_counts.entry(short.clone()).or_insert(0);
                    *count += 1;
                    if *count >= 5 {
                        return Some(format!(
                            "BLOCKED: read_file '{}' was called {} times without an edit. \
                             You already have everything this file can give you via read_file. \
                             If it's unreadable (Office binary, PDF, encoding mismatch), stop \
                             retrying and either use a bash converter (pandoc / pdftotext / \
                             antiword / unzip for .docx) or tell the user the format isn't \
                             supported. If you have enough content, act on it now.",
                            short, count
                        ));
                    }
                }
            }
        }

        // A successful edit on the same file should reset its read counter so
        // post-edit verification reads aren't blocked. `edit_file` / `write_file`
        // also clear the global recent-repeat list further down.
        if matches!(tool_name, "edit_file" | "write_file" | "create_file") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                    let short = std::path::Path::new(fp)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fp.to_string());
                    self.file_read_counts.remove(&short);
                }
            }
        }

        // --- Pattern 2: exact-repeat across any tool --------------------------
        let args_hash = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            if tool_name == "read_file" {
                // offset/limit variations are still "same file" for loop purposes.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                    if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                        fp.hash(&mut h);
                    } else {
                        args.hash(&mut h);
                    }
                } else {
                    args.hash(&mut h);
                }
            } else {
                args.hash(&mut h);
            }
            h.finish()
        };

        let sig = (tool_name.to_string(), args_hash);

        // Count repeats of this exact signature *since the last edit*. An edit
        // breaks the streak — re-issuing the same read/grep after fixing the
        // file is legitimate and must not be blocked.
        let mut repeats = 1usize; // including the current call
        for prev in self.recent_calls.iter().rev() {
            if matches!(prev.0.as_str(), "edit_file" | "write_file" | "create_file") {
                break;
            }
            if *prev == sig {
                repeats += 1;
            }
        }

        self.recent_calls.push(sig);
        if self.recent_calls.len() > 20 {
            self.recent_calls.remove(0);
        }

        if repeats >= 3 {
            return Some(format!(
                "BLOCKED: {} was called with identical arguments {} times in a row \
                 without any intervening edit. This is a loop. Read the previous error \
                 message — it explains why the call is failing. Fix the underlying \
                 problem (wrong path, wrong format, missing dependency) before retrying, \
                 or tell the user the step can't proceed.",
                tool_name, repeats
            ));
        }
        None
    }
}

/// Strip model-internal reasoning tags from streaming output.
/// Extract a file path hint from partial JSON args (e.g. `{"file_path":"/src/main.rs"`).
/// Returns the short filename on success, empty on failure. Only fires once — caller
/// should stop calling after the first hit.
fn extract_path_hint(partial_json: &str) -> Option<String> {
    // Look for "file_path":"..." or "path":"..."
    for key in &["file_path", "path"] {
        let needle = format!("\"{}\":\"", key);
        if let Some(start) = partial_json.find(&needle) {
            let val_start = start + needle.len();
            let rest = &partial_json[val_start..];
            // Find the closing quote (or take what we have so far)
            let end = rest.find('"').unwrap_or(rest.len());
            let full_path = &rest[..end];
            if !full_path.is_empty() {
                // Return just the filename or last 2 path components
                let short = full_path.rsplit('/').take(2).collect::<Vec<_>>();
                let display = short.into_iter().rev().collect::<Vec<_>>().join("/");
                return Some(display);
            }
        }
    }
    None
}

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

/// Rescue tool calls embedded as text in the model's response.
/// Some models (GLM-5 via OpenRouter) sometimes output tool calls as
/// `<tool_call>name(arg=value)</tool_call>` or `<tool_call>name(json)</tool_call>`
/// instead of using the standard function calling format.
/// Returns rescued ToolCalls, empty vec if nothing found.
fn rescue_text_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("<tool_call>") {
        let after_tag = &remaining[start + "<tool_call>".len()..];
        let end = after_tag.find("</tool_call>")
            .or_else(|| after_tag.find('\n'))
            .unwrap_or(after_tag.len());
        let body = after_tag[..end].trim();

        // Parse: "name(key=value, ...)" or "name({json})"
        if let Some(paren) = body.find('(') {
            let name = body[..paren].trim();
            let args_raw = body[paren + 1..].trim_end_matches(')').trim();

            if !name.is_empty() {
                // Try parsing as JSON first
                let args_json = if args_raw.starts_with('{') {
                    args_raw.to_string()
                } else {
                    // Convert key=value pairs to JSON
                    let mut json_parts = Vec::new();
                    for part in args_raw.split(',') {
                        let part = part.trim();
                        if let Some(eq) = part.find('=') {
                            let k = part[..eq].trim();
                            let v = part[eq + 1..].trim();
                            // Quote the value if not already quoted
                            let v_quoted = if v.starts_with('"') || v.starts_with('{') || v.starts_with('[')
                                || v == "true" || v == "false" || v.parse::<f64>().is_ok() {
                                v.to_string()
                            } else {
                                format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
                            };
                            json_parts.push(format!("\"{}\":{}", k, v_quoted));
                        }
                    }
                    format!("{{{}}}", json_parts.join(","))
                };

                let call_id = format!("rescued_{}", calls.len());
                calls.push(ToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments: args_json,
                });
            }
        }

        remaining = &after_tag[end..];
    }

    calls
}

/// Merge multiple edit_file calls targeting the same file into a single multi-edit call.
/// The model often generates 2+ separate edit_file(file, old, new) for the same file;
/// we merge them into one edit_file(file, edits=[...]) before execution.
/// Merge multiple edit_file calls on the same file into one multi-edit call.
/// Returns the ids of calls that were merged away (removed from the vec) — the caller
/// MUST emit synthetic `TurnEvent::ToolCallResult` for each of these ids, otherwise the
/// TUI sees orphan AssistantWithToolCalls entries (started but never completed) and
/// keeps spinning an in-flight icon forever (2026-04-13 "edit 完成 spinner 还在转" bug).
fn merge_edit_calls(calls: &mut Vec<ToolCall>) -> Vec<String> {
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

    if merge_targets.is_empty() { return Vec::new(); }

    let mut remove_indices: Vec<usize> = Vec::new();
    let mut removed_ids: Vec<String> = Vec::new();
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
        for &idx in &indices[1..] {
            removed_ids.push(calls[idx].id.clone());
            remove_indices.push(idx);
        }
    }

    // Remove merged calls (reverse order to preserve indices)
    remove_indices.sort_unstable();
    remove_indices.dedup();
    for idx in remove_indices.into_iter().rev() {
        calls.remove(idx);
    }

    removed_ids
}
