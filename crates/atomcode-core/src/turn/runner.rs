use std::path::{Path, PathBuf};
use std::time::Instant;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use atomcode_telemetry::{CurrentContext, Event as TelemetryEvent};

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
    /// Context construction strategy. Shared with the parent
    /// `AgentLoop::ctx` (same `Arc`) so the turn's actual send and
    /// the agent's datalog snapshot go through one ctx — per-model
    /// logic like `apply_model_directives` lands on both paths.
    /// Rebuilt on `AgentCommand::ReloadConfig` alongside the agent's
    /// clone.
    pub ctx: std::sync::Arc<dyn crate::ctx::CtxBuilder>,
    pub permission: Box<dyn PermissionDecider>,
    /// Files edited during the current session (tracked for context awareness).
    pub recently_edited_files: Vec<String>,
    /// Rolling history of `(tool_name, args_hash)` pairs — used to detect tool
    /// call loops (same tool + same args repeated without any edit in between).
    /// Bounded to 20 entries to keep memory flat. For `read_file` the hash
    /// covers `(file_path, offset, limit)` so paginating through distinct
    /// regions is not treated as a repeat; see `loop_args_hash`.
    pub recent_calls: Vec<(String, u64)>,
    /// Per-region read counter, keyed by `(basename, offset / READ_REGION_BUCKET)`.
    /// The region bucket means "scanning different parts of a large file" counts
    /// as separate keys — only reading the *same* region 3+ times in a turn is
    /// treated as a panic loop (typical of Office binaries, encoding mismatches,
    /// or the model cycling offset/limit on an unreadable file).
    pub file_read_counts: std::collections::HashMap<(String, u64), u32>,
    /// Hook executor — runs user-configured lifecycle hooks at tool execution boundaries.
    pub hook_executor: std::sync::Arc<crate::hook::executor::HookExecutor>,
}

/// Line-granularity of the read-region bucket used in `file_read_counts`.
/// A single function body typically fits in one bucket (most are < 50 lines),
/// so reading different functions of a large file produces different keys
/// and doesn't cap. Shared so `DisciplineState` and the agent loop write
/// counts under the same key the guard will read back.
pub(crate) const READ_REGION_BUCKET: u64 = 50;

/// Extract the region-bucket key for a `read_file` call so that writers
/// (agent loop, discipline) and readers (loop guard) agree on the key shape.
/// Prefer a canonical workspace-resolved path to avoid nested workspace and
/// basename collisions. Missing / malformed offset → bucket 0 (which is also
/// the bucket for "whole-file" reads).
pub(crate) fn read_region_key(args: &str, working_dir: Option<&Path>) -> (String, u64) {
    let file_key = read_file_key(args, working_dir).unwrap_or_else(|| "<malformed>".to_string());
    let offset = serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("offset").and_then(|x| x.as_u64()))
        .unwrap_or(0);
    (file_key, offset / READ_REGION_BUCKET)
}

fn read_file_key(args: &str, working_dir: Option<&Path>) -> Option<String> {
    let v = serde_json::from_str::<serde_json::Value>(args).ok()?;
    let fp = v.get("file_path").and_then(|v| v.as_str())?;
    Some(canonical_or_lexical_path_key(fp, working_dir))
}

fn canonical_or_lexical_path_key(raw_path: &str, working_dir: Option<&Path>) -> String {
    if let Some(working_dir) = working_dir {
        if let Ok(access) = crate::tool::inspect_path_access(raw_path, working_dir) {
            return access.path.display().to_string();
        }
    }

    let expanded = expand_home_for_loop_key(raw_path);
    let candidate = if expanded.is_absolute() {
        expanded
    } else if let Some(working_dir) = working_dir {
        working_dir.join(expanded)
    } else {
        expanded
    };
    lexical_normalize_for_loop_key(&candidate)
        .display()
        .to_string()
}

fn expand_home_for_loop_key(path: &str) -> PathBuf {
    if path == "~" {
        return crate::tool::real_home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return crate::tool::real_home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(path));
    }
    PathBuf::from(path)
}

fn lexical_normalize_for_loop_key(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                let can_pop = normalized
                    .components()
                    .next_back()
                    .is_some_and(|last| matches!(last, std::path::Component::Normal(_)));
                if can_pop {
                    normalized.pop();
                } else if normalized.as_os_str().is_empty() {
                    normalized.push(component.as_os_str());
                }
            }
            std::path::Component::RootDir
            | std::path::Component::Prefix(_)
            | std::path::Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }

    normalized
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
        self.run_with_filter(conversation, system_prompt, "", event_tx, cancel, None)
            .await
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
        // Telemetry: build a per-turn context carrying turn_id / provider / model.
        // Emitted on every exit path via the `tel_return!` macro below.
        let turn_id = uuid::Uuid::new_v4();
        let parent = CurrentContext::current();
        // Telemetry envelope fields:
        //   provider      = vendor type ("claude" / "openai" / "ollama"),
        //                   read directly from ProviderConfig — analytics
        //                   want the vendor label, not the user's named alias.
        //   provider_host = host parsed from base_url, with vendor default
        //                   fallback. Resolved by the telemetry crate so the
        //                   default-host table lives next to the schema.
        //   model         = LlmProvider::model_name() — the wire-level model
        //                   string sent to the API.
        let pcfg = self
            .config
            .providers
            .get(&self.config.default_provider);
        let vendor = pcfg.map(|p| p.provider_type.clone());
        let host = pcfg.and_then(|p| {
            atomcode_telemetry::resolve_provider_host(&p.provider_type, p.base_url.as_deref())
        });
        let scope_ctx = CurrentContext {
            turn_id: Some(turn_id),
            provider: parent.provider.clone().or(vendor),
            provider_host: parent.provider_host.clone().or(host),
            model: parent
                .model
                .clone()
                .or_else(|| Some(self.provider.model_name().to_string())),
            ..parent
        };
        let turn_started = std::time::Instant::now();
        // 1. Build messages within token budget.
        // Goes through `self.ctx.build_messages` (trait dispatch), NOT
        // `ctx::render::build_messages` (free fn) — otherwise per-model
        // logic like `apply_model_directives` only lands in datalog and
        // the actually-sent messages diverge from what we logged.
        let context_window = self.ctx.ctx_window();

        let (messages, ctx_stats) =
            self.ctx
                .build_messages(conversation, system_prompt, turn_reminder);

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
        let all_tool_defs = self.tools.get_definitions().await;
        let mut tool_defs: Vec<_> = if let Some(filter) = allowed_tools {
            all_tool_defs
                .into_iter()
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
                if let crate::conversation::message::MessageContent::AssistantWithToolCalls {
                    tool_calls,
                    ..
                } = &msg.content
                {
                    for call in tool_calls {
                        if call.name == "read_file" {
                            if let Ok(args) =
                                serde_json::from_str::<serde_json::Value>(&call.arguments)
                            {
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
                    let display_names: Vec<&str> = known_files
                        .iter()
                        .map(|p| p.rsplit('/').next().unwrap_or(p.as_str()))
                        .collect();
                    let list = if display_names.len() <= 6 {
                        display_names.join(", ")
                    } else {
                        format!(
                            "{}, ... ({} files)",
                            display_names[..5].join(", "),
                            display_names.len()
                        )
                    };
                    wf.description.push_str(&format!(
                        "\nThese files ALREADY EXIST — use edit_file instead: {}",
                        list,
                    ));
                }
            }
        }

        // Log the request to <datalog_dir>/llm/<ts>.json right before send.
        // `pending_request_log` holds the path so the response call below
        // can merge into the same file — passed explicitly to avoid the old
        // process-wide-static approach that bled across concurrent daemon
        // sessions.
        //
        // `datalog_dir` is resolved from `[datalog].dir` (default
        // `~/.atomcode/datalog/<project-slug>/`) — the same root the
        // markdown writer uses, so request JSON, response JSON, calls.log,
        // and the markdown summary all live next to each other for any
        // given project.
        let pending_request_log = {
            let wd = self
                .context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            let datalog_dir = crate::turn::datalog::DatalogWriter::resolve_log_dir(
                &wd,
                self.config.datalog.dir.as_deref(),
            );
            super::log::log_llm_request(
                &datalog_dir,
                &messages,
                &tool_defs,
                self.provider.model_name(),
                context_window,
                0, // step — always 0 in calls.log today; step param
                // kept for future per-tool-call correlation.
                self.config.datalog.enabled,
            )
        };

        // 3. Start streaming
        let stream_start = std::time::Instant::now();
        let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));

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
        // Telemetry: per-turn token counters populated from StreamEvent::Usage.
        let mut tel_input_tokens: u32 = 0;
        let mut tel_output_tokens: u32 = 0;
        let mut tel_cached_tokens: u32 = 0;
        let mut got_usage = false;

        // Telemetry helper: emit LlmChat (with turn_id/provider/model in scope)
        // and return the given result. `scope_ctx` is cloned for each emission so
        // the task-local is properly set when `track` reads `CurrentContext::current()`.
        macro_rules! tel_return {
            ($result:expr, $tool_count:expr, $conv:expr) => {{
                let result = $result;
                let messages_count = $conv.messages.len() as u32;
                // system_tokens: estimate from the system prompt string
                let system_tokens: u32 =
                    crate::conversation::message::Message::new(
                        crate::conversation::message::Role::System,
                        system_prompt,
                    ).estimate_tokens() as u32;
                // tool_def_tokens: direct measurement from tool definitions sent to the LLM.
                // Each ToolDef contributes name + description + JSON-serialized parameters.
                let tool_def_tokens: u32 = tool_defs
                    .iter()
                    .map(|d| {
                        let params_len = d.parameters.to_string().len();
                        // name + description + serialized params, ~4 chars/token, +4 overhead
                        (d.name.len() + d.description.len() + params_len) / 4 + 4
                    })
                    .sum::<usize>() as u32;
                // tool_result_tokens: sum of estimates for Role::Tool messages in conversation
                let tool_result_tokens: u32 = $conv
                    .messages
                    .iter()
                    .filter(|m| matches!(m.role, crate::conversation::message::Role::Tool))
                    .map(|m| m.estimate_tokens() as u32)
                    .sum();
                // message_tokens: sum of estimates for Role::User + Role::Assistant messages
                let message_tokens: u32 = $conv
                    .messages
                    .iter()
                    .filter(|m| matches!(
                        m.role,
                        crate::conversation::message::Role::User
                            | crate::conversation::message::Role::Assistant
                    ))
                    .map(|m| m.estimate_tokens() as u32)
                    .sum();
                let event = TelemetryEvent::LlmChat {
                    duration_ms: turn_started.elapsed().as_millis() as u32,
                    tool_calls_count: $tool_count as u32,
                    input_tokens: tel_input_tokens,
                    output_tokens: tel_output_tokens,
                    cached_tokens: tel_cached_tokens,
                    had_error: result.is_failed(),
                    context_window: context_window as u32,
                    system_tokens,
                    tool_def_tokens,
                    tool_result_tokens,
                    message_tokens,
                    messages_count,
                };
                let tel = self.context.telemetry.clone();
                let emit_ctx = scope_ctx.clone();
                CurrentContext::scope(emit_ctx, || async move {
                    tel.track(event);
                })
                .await;
                return result;
            }};
            // Variant for early-exit paths where conversation is not available.
            ($result:expr, $tool_count:expr) => {
                tel_return!($result, $tool_count, conversation)
            };
        }

        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => tel_return!(TurnResult::Failed(e.to_string()), 0u32),
        };
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
            let timeout = if got_any_event {
                stream_timeout
            } else {
                first_token_timeout
            };
            tokio::select! {
                            biased;

            _ = cancel.cancelled() => {
                                conversation.finalize_stream();
                                tel_return!(TurnResult::Cancelled, 0u32);
                            }

                            _ = tokio::time::sleep(timeout) => {
                                conversation.finalize_stream();
                                tel_return!(TurnResult::Failed(format!(
                                    "Stream timeout: no event for {:?}",
                                    timeout
                                )), 0u32);
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
                                        // Emit to UI for verbose mode (Ctrl+O) display.
                                        // Still accumulate for the fallback case where
                                        // content ends up empty.
                                        let _ = event_tx.send(TurnEvent::ReasoningDelta(text.clone()));
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

                                    Some(Ok(StreamEvent::ToolCallDone(mut call))) => {
                                        conversation.tool_call_buffer = None;
                                        // Variant E — atomgit gateway occasionally
                                        // corrupts `function.name` by spilling argument
                                        // attributes into it (e.g.
                                        // name='grep" path="..." pattern="..."'). The
                                        // `arguments` field is then "{}". Drop the call
                                        // entirely so it never enters tool_calls_buf nor
                                        // the assistant message — the next stream is a
                                        // fresh routing-lottery roll. Surface a one-line
                                        // Error event so the user sees what happened.
                                        if name_looks_corrupt(&call.name) {
                                            let _ = event_tx.send(TurnEvent::Error(format!(
                                                "Dropped malformed tool_call (provider returned corrupt function name: {:?})",
                                                truncate(&call.name, 60)
                                            )));
                                            continue;
                                        }

                                        // Variants A/A2/B/C/D — atomgit gateway wraps
                                        // tool args in `{"arguments": ...}` envelopes
                                        // (string- or object-valued, 1-3 levels deep,
                                        // with optional sibling fields). Schema-aware
                                        // recovery unwraps to the flat form expected by
                                        // the OpenAI tool-call protocol. See
                                        // `recover_tool_args` for the variant catalogue.
                                        let expected = self.tools.expected_top_keys(&call.name).await;
                                        if let Some(recovered) =
                                            crate::tool::recover_tool_args(&call.arguments, &expected)
                                        {
                                            call.arguments = recovered;
                                        }

                                        // ToolCallStarted is intentionally NOT sent here —
                                        // it's emitted when the tool actually starts executing,
                                        // so tool call and result are paired correctly in the
                                        // UI for sequential execution.
                                        tool_calls_buf.push(call);
                                    }

                                    Some(Ok(StreamEvent::Usage(usage))) => {
                                        total_tokens += usage.completion_tokens;
                                        // Telemetry: accumulate per-turn token counters.
                                        tel_input_tokens = tel_input_tokens.saturating_add(usage.prompt_tokens as u32);
                                        tel_output_tokens = tel_output_tokens.saturating_add(usage.completion_tokens as u32);
                                        tel_cached_tokens = tel_cached_tokens.saturating_add(usage.cached_tokens as u32);
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

                                        // Normalize tool calls before they enter history. In
                                        // particular, merging same-file edit_file calls after
                                        // finalization leaves the assistant message declaring
                                        // more tool calls than the ToolResults we later append,
                                        // which poisons the next provider request.
                                        merge_edit_calls(&mut tool_calls_buf);

                                        // Finalize conversation state. Pass the accumulated
                                        // reasoning_buf so thinking-model providers (Moonshot
                                        // Kimi K2-thinking/K2.6, etc.) can echo it back on
                                        // the next request — without this the provider 400s
                                        // with "reasoning_content is missing in assistant
                                        // tool call message". The send-side ReasoningPolicy
                                        // (per-provider) decides whether the field actually
                                        // reaches the wire.
                                        if !tool_calls_buf.is_empty() {
                                            let reasoning = if reasoning_buf.trim().is_empty() {
                                                None
                                            } else {
                                                Some(reasoning_buf.as_str())
                                            };
                                            conversation.finalize_stream_with_tool_calls(
                                                &tool_calls_buf,
                                                reasoning,
                                            );
                                        } else {
                                            conversation.finalize_stream();
                                        }
                                        was_truncated = is_truncated;
                                        break;
                                    }

                                    Some(Ok(StreamEvent::Error(e))) => {
                                        conversation.finalize_stream();
                                        tel_return!(TurnResult::Failed(e), 0u32);
                                    }

                                    Some(Err(e)) => {
                                        conversation.finalize_stream();
                                        tel_return!(TurnResult::Failed(e.to_string()), 0u32);
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

        // Log LLM response (text + tool calls) into the same per-project
        // datalog dir as the request — see comment on the matching
        // `log_llm_request` call above.
        let response_duration = stream_start.elapsed().as_millis() as u64;
        let wd = self
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        let datalog_dir = crate::turn::datalog::DatalogWriter::resolve_log_dir(
            &wd,
            self.config.datalog.dir.as_deref(),
        );
        super::log::log_llm_response(
            &datalog_dir,
            pending_request_log,
            &text_buf,
            &tool_calls_buf,
            &reasoning_buf,
            self.provider.model_name(),
            0, // step is set by caller
            response_duration,
            self.config.datalog.enabled,
        );

        if tool_calls_buf.is_empty() && text_buf.trim().is_empty() {
            tel_return!(
                TurnResult::Failed(
                    "Provider returned an empty response (no text, no tool calls).".to_string(),
                ),
                0u32
            );
        }

        // 5. If no tool calls, we're done — LLM produced text only
        if tool_calls_buf.is_empty() {
            tel_return!(
                TurnResult::Responded {
                    text: text_buf,
                    tokens: total_tokens,
                    truncated: was_truncated,
                },
                0u32
            );
        }

        // 6. Tool calls were normalized before being written into conversation
        // history. From this point on, execute exactly the calls the provider
        // will see in the assistant message on the next turn.
        //
        // Auto-merge multiple edit_file calls on the same file into one multi-edit.
        // Models often generate 2+ separate edit_file calls for the same file instead of
        // using the edits array. Merging at framework level is 100% reliable vs prompt ~50%.

        // ── Layer B: per-turn read budget allocation ──
        // Count read_file calls in this batch and set per-file token budget.
        // Formula: 20% of ctx budget / num_reads. This ensures N reads in one
        // turn share the budget fairly — 1 read gets 20%, 3 reads get 6.7% each.
        // read.rs Layer A checks file_tokens against this to decide full vs skeleton.
        {
            let num_reads = tool_calls_buf
                .iter()
                .filter(|c| c.name == "read_file")
                .count()
                .max(1); // avoid division by zero
            let budget = self
                .context
                .ctx_budget_hint
                .load(std::sync::atomic::Ordering::Relaxed);
            let per_file = budget / (5 * num_reads);
            self.context.read_budget_tokens.store(
                per_file.max(2000), // floor: ~170 lines always get full content
                std::sync::atomic::Ordering::Relaxed,
            );
        }

        let tool_count = tool_calls_buf.len();
        let mut seen_calls: std::collections::HashMap<(String, String), usize> =
            std::collections::HashMap::new();
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
                tel_return!(TurnResult::Cancelled, tool_count);
            }

            // ── Loop detection: gate BEFORE rendering anything ──
            // detect_call_loop must run before ToolCallStarted is emitted,
            // otherwise blocked attempts (3rd+ consecutive identical call)
            // each create a UiLine::ToolCallInFlight row in scrollback —
            // visible as a wall of identical `Bash(cd ... && cargo ...)`
            // rows when the model gets stuck. Skipping the Started emit
            // keeps blocked calls invisible (the BLOCKED ToolCallResult
            // still flows back to the model so it can react / give up).
            //
            // We use the raw (pre-name-canonicalization, pre-args-repair)
            // call signature here. That means an alias-spamming model that
            // alternates `Run` → `bash` → `run` would not trip the guard
            // since the hash differs each time — this is rare in practice
            // and the previous post-canonicalization check inside
            // execute_single_tool also missed this case once the alias
            // resolved to the same canonical name (it canonicalized the
            // name in-place, so `Run` and `bash` did hash differently
            // there too).
            if let Some(msg) = self.detect_call_loop(&call.name, &call.arguments) {
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: msg.clone(),
                    success: false,
                    duration: std::time::Duration::ZERO,
                });
                conversation.add_tool_result(ToolResult {
                    call_id: call.id.clone(),
                    output: msg,
                    success: false,
                });
                continue;
            }

            // Send ToolCallStarted event when the tool actually starts executing.
            // This ensures tool call and result are paired correctly in the UI.
            let _ = event_tx.send(TurnEvent::ToolCallStarted {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            });

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

        // Truncate oversized tool outputs before returning. Without this,
        // a single `ls -la node_modules` / wide `find` dump (multi-MB)
        // stays raw in `conversation.messages` and the NEXT LLM call
        // blows the upstream context limit. Every caller of TurnRunner
        // used to have to remember to invoke this — daemon didn't, which
        // was the root of the 738K-token 400 bug. Making runner own it
        // removes the implicit contract.
        crate::ctx::truncate::post_process_tool_results(
            &mut conversation.messages,
            tool_count,
            "", // fallback only — each result is keyed by its own
            // call_id → ATC.tool_name lookup (see ctx::truncate).
            context_window,
        );

        tel_return!(
            TurnResult::UsedTools {
                text: if text_buf.is_empty() {
                    None
                } else {
                    Some(text_buf)
                },
                tool_count,
                tokens: total_tokens,
            },
            tool_count
        );
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
        let result = self
            .run_with_filter(
                &mut mini_conv,
                system_prompt,
                "",
                event_tx,
                cancel,
                Some(execute_tools),
            )
            .await;

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
            "run" | "run_command" | "run_server" | "run_shell" | "run_app" | "execute"
            | "shell" | "terminal" => "bash",
            "list_files" | "ls" => "list_directory",
            "search" => "grep",
            _ => "",
        };
        let corrected_name = if corrected_name.is_empty() {
            // No alias match — try case-insensitive lookup in registry
            if self.tools.get(&call.name).await.is_some() {
                call.name.clone()
            } else if let Some(name) = self.tools.iter().await
                .find(|(k, _)| k.eq_ignore_ascii_case(&call.name))
                .map(|(k, _)| k)
            {
                name
            } else {
                call.name.clone()
            }
        } else {
            corrected_name.to_string()
        };
        // Clone the Arc so the borrow of `self.tools` ends here — we need to
        // call `self.detect_call_loop(..)` mutably below.
        let tool = match self.tools.get(&corrected_name).await {
            Some(t) => t,
            None => {
                let available: String = self.tools.iter().await
                    .map(|(name, _)| name)
                    .collect::<Vec<String>>()
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
        let repaired_args = super::json_repair::repair_tool_args(&corrected_name, &call.arguments);

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

        // Loop detection moved upstream to `dispatch_tools` (gates BEFORE
        // ToolCallStarted is emitted, so blocked attempts don't render
        // ghost inflight rows in scrollback). When we reach here the
        // call has already cleared that guard exactly once.

        // Check permission via the injected PermissionDecider.
        // AutoApprove tools execute immediately; RequireApproval tools go through
        // the decider which handles interactive prompts or automatic policy.
        let approval = tool.approval_with_context(&call.arguments, &self.context);
        if let crate::tool::ApprovalRequirement::RequireApproval(ref reason)
        | crate::tool::ApprovalRequirement::RequireApprovalAlways(ref reason) = approval
        {
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

        // --- PreToolUse Hook ---
        if self.hook_executor.has_hooks() {
            let hook_ctx = self.build_hook_context(
                "pre_tool_use",
                Some(&call.name),
                Some(&call.arguments),
                None,
                None,
            );
            let pre_result = self.hook_executor.run_pre_tool_use(&call.name, &hook_ctx).await;
            match pre_result {
                crate::hook::PreHookResult::Block { reason } => {
                    let output = format!("Blocked by hook: {}", reason);
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
                crate::hook::PreHookResult::Modify { .. } => {
                    // Modify support deferred — treat as Allow
                }
                crate::hook::PreHookResult::Allow => {}
            }
        }

        // Snapshot the shared working directory before executing. Tools like
        // `change_dir` and `bash` (when the command starts with `cd`) mutate
        // `ctx.working_dir` in place; we compare before/after to emit a
        // `WorkingDirChanged` event so the TUI footer can track the cwd
        // without polling the `Arc<RwLock<PathBuf>>` every frame.
        let wd_before = self.context.working_dir.read().await.clone();

        // Set up event sender for real-time tool output streaming
        self.context.event_tx = Some(std::sync::Arc::new(event_tx.clone()));
        self.context.current_call_id = Some(call.id.clone());

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
                // Clean up event sender
                self.context.event_tx = None;
                self.context.current_call_id = None;

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

        // Clean up event sender after tool execution
        self.context.event_tx = None;
        self.context.current_call_id = None;

        let duration = start.elapsed();

        // If the tool mutated the shared working directory, surface it as
        // a TurnEvent so the TUI layer can keep its footer in sync. Emit
        // before ToolCallResult so consumers that redraw on result see
        // the new cwd in the same frame.
        let wd_after = self.context.working_dir.read().await.clone();
        if wd_after != wd_before {
            let _ = event_tx.send(TurnEvent::WorkingDirChanged(wd_after));
        }

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

        // --- PostToolUse Hook ---
        if self.hook_executor.has_hooks() {
            let hook_ctx = self.build_hook_context(
                "post_tool_use",
                Some(&call.name),
                Some(&call.arguments),
                Some(&tool_result.output),
                Some(tool_result.success),
            );
            self.hook_executor.run_post_tool_use(&call.name, &hook_ctx).await;
        }

        let _ = event_tx.send(TurnEvent::ToolCallResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: tool_result.output.clone(),
            success: tool_result.success,
            duration,
        });

        tool_result
    }

    fn build_hook_context(
        &self,
        event: &str,
        tool_name: Option<&str>,
        tool_args: Option<&str>,
        tool_result: Option<&str>,
        tool_success: Option<bool>,
    ) -> crate::hook::HookContext {
        let wd = self
            .context
            .working_dir
            .try_read()
            .map(|g| g.display().to_string())
            .unwrap_or_default();
        crate::hook::HookContext {
            event: event.into(),
            tool_name: tool_name.map(String::from),
            tool_args: tool_args.and_then(|a| serde_json::from_str(a).ok()),
            tool_result: tool_result.map(String::from),
            tool_success,
            session_id: String::new(),
            working_dir: wd,
        }
    }

    /// Detect tool-call loops and return a recovery message when one should be
    /// blocked. Also updates the rolling call history as a side effect.
    ///
    /// Two patterns are caught:
    ///
    /// 1. **Per-region read saturation** (`read_file` specific):
    ///    3 unbroken `read_file` calls against the *same region* of a file
    ///    (basename + offset bucket). Paginating through distinct regions of
    ///    a large file produces different keys and does NOT trip — this
    ///    specifically targets the panic loop where the model re-reads the
    ///    same slice hoping for different content (Office binary, encoding
    ///    mismatch, etc.). Reset by a successful `edit_file` / `write_file`
    ///    targeting the same file (clears ALL regions for that file).
    ///
    /// 2. **Exact repeats** (any tool): 3 calls with identical `(tool_name,
    ///    args_hash)` and no intervening `edit_file` / `write_file`. Means
    ///    the model re-issued the same command without reacting to the
    ///    previous failure.
    ///
    /// For `read_file`, the hash covers `(file_path, offset, limit)` so that
    /// paginating through distinct regions of a large file does NOT count as
    /// a loop — only literal re-reads do.
    pub(super) fn detect_call_loop(&mut self, tool_name: &str, args: &str) -> Option<String> {
        // --- Pattern 1: per-region read saturation ----------------------------
        let working_dir = self.context.working_dir.try_read().ok().map(|g| g.clone());
        if tool_name == "read_file" {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                    let display_name = std::path::Path::new(fp)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| fp.to_string());
                    let key = read_region_key(args, working_dir.as_deref());
                    let count = self.file_read_counts.entry(key).or_insert(0);
                    *count += 1;
                    if *count >= 3 {
                        return Some(format!(
                            "BLOCKED: read_file '{}' hit its {}-call cap for the SAME region of this file. \
                             You keep requesting the same slice and getting the same output. \
                             First, check that the path itself is right — if you're using a relative \
                             or basename-only path, the user may have mentioned a specific absolute \
                             path (e.g. ~/some/dir/file) that you should be using instead. \
                             If you need more of this file, pass a different offset to jump elsewhere. \
                             If you're stuck because the file is unreadable (Office binary, PDF, \
                             encoding mismatch), switch to a bash converter \
                             (pandoc / pdftotext / antiword / unzip for .docx) or tell the user \
                             the format isn't supported. \
                             Do not re-read this region again in this turn.",
                            display_name, count
                        ));
                    }
                }
            }
        }

        // A successful edit on a file clears ALL of that file's region counts,
        // so post-edit verification reads (potentially covering different parts)
        // aren't blocked. `edit_file` / `write_file` also clear the global
        // recent-repeat list further down.
        if matches!(tool_name, "edit_file" | "write_file" | "create_file") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = v.get("file_path").and_then(|v| v.as_str()) {
                    let file_key = canonical_or_lexical_path_key(fp, working_dir.as_deref());
                    self.file_read_counts
                        .retain(|(file, _), _| file != &file_key);
                }
            }
        }

        // --- Pattern 2: exact-repeat across any tool --------------------------
        let args_hash = loop_args_hash(tool_name, args, working_dir.as_deref());
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

/// Hash a tool call for exact-repeat loop detection.
///
/// For `read_file` we hash `(file_path, offset, limit)` — paginating through
/// different regions of the same large file must NOT collapse to one hash
/// (that was the historical behavior and it tripped the 3-repeat guard on
/// legitimate scans). Missing `offset` / `limit` normalize to 0 so the hash
/// is stable whether the model omits the field or sends it as `null`.
///
/// For every other tool we hash the whole `args` string.
fn loop_args_hash(tool_name: &str, args: &str, working_dir: Option<&Path>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if tool_name == "read_file" {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
            if v.get("file_path").and_then(|v| v.as_str()).is_some() {
                let file_key = read_file_key(args, working_dir).unwrap_or_default();
                file_key.hash(&mut h);
                v.get("offset")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .hash(&mut h);
                v.get("limit")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .hash(&mut h);
                return h.finish();
            }
        }
        // Malformed args or missing file_path — hash raw so identical bad
        // calls still collapse and trip the loop detector.
        args.hash(&mut h);
    } else {
        args.hash(&mut h);
    }
    h.finish()
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

/// Detect provider-side corruption of a tool_call's `function.name` field.
/// atomgit's gateway sometimes spills attribute syntax into `name`, leaving
/// `arguments` as `"{}"` — e.g. `name='grep" path="..." pattern="..."'`.
/// Legitimate tool names are short ASCII identifiers (`bash`, `read_file`,
/// `mcp__server__tool`), so any whitespace/quote/`=`/`<`/`>` or a length
/// far above what we register is a strong corruption signal.
fn name_looks_corrupt(name: &str) -> bool {
    if name.is_empty() {
        return true;
    }
    if name.len() > 96 {
        return true;
    }
    name.chars().any(|c| c.is_whitespace() || matches!(c, '"' | '=' | '<' | '>'))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
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
        let end = after_tag
            .find("</tool_call>")
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
                            let v_quoted = if v.starts_with('"')
                                || v.starts_with('{')
                                || v.starts_with('[')
                                || v == "true"
                                || v == "false"
                                || v.parse::<f64>().is_ok()
                            {
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

/// Merge multiple edit_file calls on the same file into one multi-edit call.
/// The model often generates 2+ separate edit_file(file, old, new) for the same file;
/// we merge them into one edit_file(file, edits=[...]) before execution and before
/// the assistant tool-call message is written into conversation history.
/// Returns the ids of calls that were merged away.
fn merge_edit_calls(calls: &mut Vec<ToolCall>) -> Vec<String> {
    use std::collections::HashMap;

    // Group edit_file calls by file_path. Preserve order of first occurrence.
    let mut file_groups: HashMap<String, Vec<usize>> = HashMap::new();
    let mut file_order: Vec<String> = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        if call.name != "edit_file" {
            continue;
        }
        let fp = serde_json::from_str::<serde_json::Value>(&call.arguments)
            .ok()
            .and_then(|a| {
                a.get("file_path")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });
        if let Some(fp) = fp {
            let entry = file_groups.entry(fp.clone()).or_default();
            if entry.is_empty() {
                file_order.push(fp);
            }
            entry.push(i);
        }
    }

    // Only merge groups with 2+ calls
    let merge_targets: Vec<(String, Vec<usize>)> = file_order
        .into_iter()
        .filter_map(|fp| {
            let indices = file_groups.remove(&fp)?;
            if indices.len() >= 2 {
                Some((fp, indices))
            } else {
                None
            }
        })
        .collect();

    if merge_targets.is_empty() {
        return Vec::new();
    }

    let mut remove_indices: Vec<usize> = Vec::new();
    let mut removed_ids: Vec<String> = Vec::new();
    for (file_path, indices) in &merge_targets {
        // Build edits array from individual calls
        let mut edits: Vec<serde_json::Value> = Vec::new();
        for &idx in indices {
            let args: serde_json::Value =
                serde_json::from_str(&calls[idx].arguments).unwrap_or_default();
            let mut edit = serde_json::Map::new();
            if let Some(v) = args.get("old_string") {
                edit.insert("old_string".into(), v.clone());
            }
            if let Some(v) = args.get("new_string") {
                edit.insert("new_string".into(), v.clone());
            }
            if let Some(v) = args.get("start_line") {
                edit.insert("start_line".into(), v.clone());
            }
            if let Some(v) = args.get("end_line") {
                edit.insert("end_line".into(), v.clone());
            }
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

#[cfg(test)]
mod loop_hash_tests {
    use super::{loop_args_hash, read_region_key, READ_REGION_BUCKET};

    // Using a separate module name to avoid conflicting with the sibling
    // `turn::tests` integration-style test module.

    #[test]
    fn read_file_hash_distinguishes_different_windows() {
        // The core fix: hashing must make paginated reads appear as distinct
        // calls, otherwise the 3-repeat guard fires on legitimate scans of a
        // single large file. If this test fails, the model is about to be
        // blocked after 3 offsets.
        let a = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":60}"#,
            None,
        );
        let b = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":100,"limit":60}"#,
            None,
        );
        assert_ne!(a, b, "offsets 1 vs 100 must hash differently");

        let c = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":60}"#,
            None,
        );
        let d = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":120}"#,
            None,
        );
        assert_ne!(c, d, "limit 60 vs 120 must hash differently");

        let e = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":60}"#,
            None,
        );
        let f = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":60}"#,
            None,
        );
        assert_eq!(e, f, "identical args must hash identically");

        let g = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":1,"limit":60}"#,
            None,
        );
        let h = loop_args_hash(
            "read_file",
            r#"{"file_path":"/b.rs","offset":1,"limit":60}"#,
            None,
        );
        assert_ne!(g, h, "different files must hash differently");
    }

    #[test]
    fn missing_offset_and_limit_normalize_to_zero() {
        // `{path}` and `{path, offset:0, limit:0}` must hash the same — otherwise
        // the model can evade the loop guard just by toggling the field's presence.
        let bare = loop_args_hash("read_file", r#"{"file_path":"/a.rs"}"#, None);
        let zeros = loop_args_hash(
            "read_file",
            r#"{"file_path":"/a.rs","offset":0,"limit":0}"#,
            None,
        );
        assert_eq!(bare, zeros);
    }

    #[test]
    fn other_tools_hash_full_args() {
        // Non-read tools keep full-args hashing so changing any field (path,
        // pattern, command) is correctly treated as a different call.
        let a = loop_args_hash("grep", r#"{"pattern":"foo","path":"/x"}"#, None);
        let b = loop_args_hash("grep", r#"{"pattern":"foo","path":"/y"}"#, None);
        assert_ne!(a, b);

        let s1 = loop_args_hash("bash", r#"{"command":"ls"}"#, None);
        let s2 = loop_args_hash("bash", r#"{"command":"ls"}"#, None);
        assert_eq!(s1, s2);
    }

    #[test]
    fn region_key_buckets_are_per_file() {
        // Same file, same bucket regardless of how offset rounds down.
        let a = read_region_key(
            r#"{"file_path":"/x/render.rs","offset":100,"limit":50}"#,
            None,
        );
        let b = read_region_key(
            r#"{"file_path":"/x/render.rs","offset":130,"limit":50}"#,
            None,
        );
        assert_eq!(
            a,
            b,
            "offsets 100 and 130 both land in bucket {}",
            100 / READ_REGION_BUCKET
        );

        // Jump by one full bucket → different key.
        let far = read_region_key(
            &format!(
                r#"{{"file_path":"/x/render.rs","offset":{},"limit":50}}"#,
                READ_REGION_BUCKET + 200
            ),
            None,
        );
        assert_ne!(a, far, "offsets across bucket boundaries must differ");

        // Whole-file read and `offset=0` both normalize to bucket 0.
        let full = read_region_key(r#"{"file_path":"/x/render.rs"}"#, None);
        let zero = read_region_key(r#"{"file_path":"/x/render.rs","offset":0}"#, None);
        assert_eq!(full, zero);
        assert_eq!(full.1, 0);

        // Different files are different keys even at the same offset bucket.
        let other = read_region_key(r#"{"file_path":"/x/mod.rs","offset":100}"#, None);
        assert_ne!(a, other);
    }

    #[test]
    fn read_file_hash_canonicalizes_relative_paths_with_working_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn lib() {}\n").unwrap();

        let rel = loop_args_hash(
            "read_file",
            r#"{"file_path":"src/lib.rs","offset":0,"limit":20}"#,
            Some(dir.path()),
        );
        let abs = loop_args_hash(
            "read_file",
            &format!(
                r#"{{"file_path":"{}","offset":0,"limit":20}}"#,
                file.display()
            ),
            Some(dir.path()),
        );

        assert_eq!(rel, abs, "relative and absolute path to same file must collapse");
    }

    #[test]
    fn region_key_distinguishes_same_basename_in_different_dirs() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a/mod.rs");
        let b = dir.path().join("b/mod.rs");
        std::fs::create_dir_all(a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(b.parent().unwrap()).unwrap();
        std::fs::write(&a, "mod a;\n").unwrap();
        std::fs::write(&b, "mod b;\n").unwrap();

        let key_a = read_region_key(
            r#"{"file_path":"a/mod.rs","offset":100,"limit":50}"#,
            Some(dir.path()),
        );
        let key_b = read_region_key(
            r#"{"file_path":"b/mod.rs","offset":100,"limit":50}"#,
            Some(dir.path()),
        );

        assert_ne!(key_a, key_b, "same basename in different dirs must not share a read bucket");
    }

    #[test]
    fn region_key_handles_malformed_args() {
        // Garbage in → bucket 0 (same as no-offset), so at worst we over-count
        // a single bucket and the model gets a helpful block message instead
        // of a mis-routed one.
        let bad = read_region_key("not json", None);
        assert_eq!(bad, ("<malformed>".to_string(), 0));
    }

    #[test]
    fn malformed_read_args_fall_back_to_raw_hash() {
        // If args aren't valid JSON or lack file_path, still produce a stable
        // hash from the raw string so the loop detector at least collapses
        // exact duplicate bad calls.
        let a = loop_args_hash("read_file", "not json at all", None);
        let b = loop_args_hash("read_file", "not json at all", None);
        assert_eq!(a, b);

        let c = loop_args_hash("read_file", r#"{"no_file_path":"oops"}"#, None);
        let d = loop_args_hash("read_file", r#"{"no_file_path":"oops"}"#, None);
        assert_eq!(c, d);

        assert_ne!(a, c, "different malformed inputs still differ");
    }
}
