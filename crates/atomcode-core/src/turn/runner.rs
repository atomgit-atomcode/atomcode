use std::time::Instant;

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use atomcode_telemetry::{CurrentContext, Event as TelemetryEvent, LlmErrorKind, ToolErrorKind};

use crate::config::Config;
use crate::conversation::Conversation;
use crate::hook::{HookCtx, HookEngine, ToolResultContext};
use crate::provider::LlmProvider;
use crate::stream::StreamEvent;
use crate::tool::{
    PermissionDecision, ToolCall, ToolCallBuffer, ToolContext, ToolRegistry, ToolResult,
};

use super::event::{TurnEvent, TurnResult};
use super::loop_guard::{LoopGuardDecision, LoopGuardState};
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
    /// 统一 Hook 引擎 (Arc, 由 AgentLoop 管理)
    pub hook_engine: std::sync::Arc<HookEngine>,
    /// Files edited during the current session (tracked for context awareness).
    pub recently_edited_files: Vec<String>,
    /// Cross-batch tool-call loop guard. Cleared per user-message by the
    /// agent (see `handle_send_message`); records every executed tool's
    /// `(name, args, output_hash)` triple and short-circuits the third
    /// identical attempt. See `loop_guard.rs` for the full rationale.
    pub loop_guard: LoopGuardState,
    /// Current turn number, set by AgentLoop before each turn.
    /// Propagated to ToolCallStartContext so built-in hooks (e.g.
    /// ToolAuditLogHook) see the correct turn index.
    pub current_turn_number: u32,
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
        let pcfg = self.config.providers.get(&self.config.default_provider);
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

        // Commit-collapse old tool results (idempotent, monotonic) so the
        // sent prefix stays byte-stable across turns and the provider
        // prompt-cache holds. Replaces the removed ephemeral microcompact.
        crate::ctx::render::collapse_committed(conversation, context_window);

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
        // `$ATOMCODE_HOME/datalog/<project-slug>/`) — the same root the
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
                &self.provider.session_id(),
                self.config.datalog.enabled,
            )
        };

        // 3. Start streaming
        let stream_start = std::time::Instant::now();
        let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));

        // 4. Process stream events
        let mut tool_calls_buf: Vec<ToolCall> = Vec::new();
        // RAW accumulator — keeps `<tool_call>...</tool_call>` blocks intact
        // so the rescue path at Done can parse them when the model emitted
        // its tool calls as XML in text instead of using the structured
        // tool_calls API (Qwen / GLM / DeepSeek occasional misbehavior).
        let mut text_buf = String::new();
        // VISIBLE accumulator — mirror of what `stream_filter` actually
        // emitted to UI / conversation history. Used for `TurnResult::
        // Responded.text` so downstream consumers (datalog `log_text`,
        // ATLAS plan extraction, telemetry) see the same clean text the
        // user saw, not the raw text_buf with leaked XML. Earlier bug
        // (5-7 datalog 20-14-23 Turn 5): Responded.text was raw text_buf
        // → datalog `**Response:**` block carried `<tool_call>grep<arg_key>
        // pattern</arg_key>...</tool_call>` mid-prose, polluting A/B
        // analysis.
        let mut visible_text_buf = String::new();
        // Runaway-stream guard: if a degenerate model gets stuck in an
        // autoregressive loop emitting the same character forever, we
        // abort the turn rather than burning the user's quota waiting
        // for the upstream max_tokens cap (#225). The detector keeps
        // running across all visible Delta text in this turn — tool
        // call boundaries reset stream_filter but not this; a real
        // runaway will still trip on the per-segment text alone.
        let mut runaway_detector = crate::stream::RunawayDetector::with_default_threshold();
        // Reasoning-model thinking content collected separately — not emitted
        // to scrollback by default (users don't want to read the thinking).
        // If `text_buf` ends up empty at `Done` but this is non-empty, we
        // promote reasoning to the final answer: some gateways route entire
        // responses through `reasoning_content` for MiniMax-M2.7 / DeepSeek-R1,
        // and without the fallback we'd return a silent 0-token "Nailed it".
        let mut reasoning_buf = String::new();
        // Anthropic extended-thinking blocks (text + signature) accumulated
        // from `StreamEvent::ThinkingBlock`. Carried into the message via
        // `finalize_stream_with_tool_calls_and_thinking` so the next
        // request can echo them back — Anthropic 400s otherwise (`The
        // content[].thinking in the thinking mode must be passed back`).
        let mut thinking_blocks: Vec<crate::conversation::message::ThinkingBlock> = Vec::new();
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
                let system_tokens: u32 = crate::conversation::message::Message::new(
                    crate::conversation::message::Role::System,
                    system_prompt,
                )
                .estimate_tokens() as u32;
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
                    .filter(|m| {
                        matches!(
                            m.role,
                            crate::conversation::message::Role::User
                                | crate::conversation::message::Role::Assistant
                        )
                    })
                    .map(|m| m.estimate_tokens() as u32)
                    .sum();
                let (error_kind, error_data) = if result.is_failed() {
                    let reason = match &result {
                        TurnResult::Failed(r) => r.clone(),
                        _ => String::new(),
                    };
                    let kind = classify_llm_error(&reason);
                    let error_data = build_llm_error_data(
                        kind,
                        &reason,
                        turn_started.elapsed().as_millis() as u32,
                        scope_ctx.provider.as_deref(),
                        scope_ctx.provider_host.as_deref(),
                        scope_ctx.model.as_deref(),
                        context_window as u32,
                        system_tokens,
                        tool_def_tokens,
                        tool_result_tokens,
                        message_tokens,
                        messages_count,
                    );
                    (Some(kind), error_data)
                } else {
                    (None, None)
                };
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
                    error_kind,
                    error_data,
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
        // Hides `<tool_call>...</tool_call>` blocks from UI/conversation while
        // keeping `text_buf` raw so rescue can still parse them at Done.
        let mut stream_filter = ToolCallStreamFilter::default();

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
                                let reasoning = reasoning_opt(&reasoning_buf);
                                conversation.finalize_stream_with_reasoning(
                                    reasoning.as_deref(),
                                    std::mem::take(&mut thinking_blocks),
                                );
                                tel_return!(TurnResult::Cancelled, 0u32);
                            }

                            _ = tokio::time::sleep(timeout) => {
                                let reasoning = reasoning_opt(&reasoning_buf);
                                conversation.finalize_stream_with_reasoning(
                                    reasoning.as_deref(),
                                    std::mem::take(&mut thinking_blocks),
                                );
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
                                            // Raw goes into rescue source so XML tool_call blocks
                                            // can be parsed at Done.
                                            text_buf.push_str(&text);
                                            // Visible stream excludes <tool_call>...</tool_call>
                                            // blocks (Qwen/GLM XML leak suppression).
                                            let visible = stream_filter.feed(&text);
                                            if !visible.is_empty() {
                                                // Runaway guard runs on the visible stream
                                                // only — XML scaffolding inside <tool_call>
                                                // blocks legitimately runs long, and we
                                                // don't want to false-positive on it (#225).
                                                if let Some(reason) =
                                                    runaway_detector.feed(&visible)
                                                {
                                                    conversation.push_delta(&visible);
                                                    visible_text_buf.push_str(&visible);
                                                    let _ = event_tx
                                                        .send(TurnEvent::TextDelta(visible));
                                                    let reasoning = reasoning_opt(&reasoning_buf);
                                                    conversation.finalize_stream_with_reasoning(
                                                        reasoning.as_deref(),
                                                        std::mem::take(&mut thinking_blocks),
                                                    );
                                                    tel_return!(
                                                        TurnResult::Failed(reason),
                                                        0u32
                                                    );
                                                }
                                                conversation.push_delta(&visible);
                                                visible_text_buf.push_str(&visible);
                                                let _ = event_tx.send(TurnEvent::TextDelta(visible));
                                            }
                                        }
                                    }
                                    Some(Ok(StreamEvent::Reasoning(text))) => {
                                        got_any_event = true;
                                        // Emit to UI for verbose mode (Ctrl+O) display,
                                        // but do not surface known provider filler.
                                        let text = strip_reasoning_filler(&text);
                                        if !text.is_empty() {
                                            let _ =
                                                event_tx.send(TurnEvent::ReasoningDelta(text.clone()));
                                            reasoning_buf.push_str(&text);
                                        }
                                    }
                                    Some(Ok(StreamEvent::ThinkingBlock { text, signature })) => {
                                        got_any_event = true;
                                        // Anthropic-only path: store the block (with
                                        // its signature) for echo-back. Don't emit a
                                        // UI event — the text was already streamed
                                        // through ReasoningDelta during the deltas.
                                        thinking_blocks.push(
                                            crate::conversation::message::ThinkingBlock {
                                                text,
                                                signature,
                                            },
                                        );
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
                                        // Flush any holdback from the tool_call filter. If the
                                        // stream ended mid-`<tool_call>` block, the filter
                                        // discards the partial — preferring a missing close to
                                        // a leaked tag.
                                        let trailing = stream_filter.flush();
                                        if !trailing.is_empty() {
                                            conversation.push_delta(&trailing);
                                            visible_text_buf.push_str(&trailing);
                                            let _ = event_tx.send(TurnEvent::TextDelta(trailing));
                                        }

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
                                            // Repair path: model split intent across two channels
                                            // — function-calling JSON arrived with truncated args
                                            // (e.g. only `new_string`, missing `old_string`),
                                            // while the text stream carried the complete args as
                                            // `<tool_call>` XML. Fill missing keys from the XML
                                            // pool so the call doesn't fail at execute() with a
                                            // misleading "old_string is required". JSON wins on
                                            // conflicts; XML only fills gaps.
                                            let xml_pool = rescue_text_tool_calls(&text_buf);
                                            if !xml_pool.is_empty() {
                                                repair_tool_call_args(&mut tool_calls_buf, &xml_pool);
                                            }
                                            false
                                        };

                                        if text_buf.trim().is_empty()
                                            && tool_calls_buf.is_empty()
                                            && !rescued_tools
                                            && !is_only_placeholder_filler(&reasoning_buf)
                                        {
                                            // Skip-promotion guard: when the reasoning
                                            // channel carries nothing besides copies of
                                            // our own outbound placeholder
                                            // (`(no reasoning recorded)`), don't promote
                                            // it to the assistant text channel. Some
                                            // gateways echo back the placeholder as the
                                            // response's reasoning_content; more often
                                            // the model mimics the pattern from a
                                            // context full of historical placeholder
                                            // copies — DeepSeek V4 thinking-mode
                                            // requires non-empty reasoning_content on
                                            // every historical assistant tool_call
                                            // message, so a 17-round session has 17
                                            // copies of the placeholder in context, and
                                            // the response often comes back as 3+
                                            // copies concatenated. `is_only_placeholder_filler`
                                            // handles any N (≥1) copies plus
                                            // interleaved whitespace. Promoting that
                                            // would commit a meaningless string to
                                            // history AND present `Responded { text:
                                            // "(no reasoning recorded)..." }` to the
                                            // agent loop, which then calls
                                            // finish_turn(Natural) and the user sees a
                                            // silent "Nailed it" mid-task stop
                                            // (user-reported on DeepSeek V4 Flash, 17
                                            // rounds 20 tools, screenshot showed the
                                            // placeholder as the only assistant text
                                            // before TurnComplete fired). With the
                                            // guard: text_buf stays empty, falls
                                            // through to the empty-response Failed
                                            // branch below, the agent loop's existing
                                            // 3-retry-with-backoff path takes over and
                                            // surfaces the issue to the user instead of
                                            // burying it as success.
                                            // is_only_placeholder_filler already routed
                                            // PURE-placeholder buffers to the empty-response
                                            // path above, so we only reach here with REAL
                                            // content — but a mimicking thinking model can
                                            // splice placeholder copies INTO that content
                                            // (mixed case). Strip them so the sentinel never
                                            // surfaces in the user-visible answer.
                                            let promoted = strip_reasoning_filler(&std::mem::take(
                                                &mut reasoning_buf,
                                            ));
                                            conversation.push_delta(&promoted);
                                            text_buf.push_str(&promoted);
                                            // Reasoning channel doesn't carry tool_call XML
                                            // (it's a separate stream from delta text), so
                                            // promoting it directly to visible_text_buf is
                                            // safe — no need to re-feed through stream_filter.
                                            visible_text_buf.push_str(&promoted);
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
                                        let reasoning = reasoning_opt(&reasoning_buf);
                                        if !tool_calls_buf.is_empty() {
                                            conversation
                                                .finalize_stream_with_tool_calls_and_thinking(
                                                    &tool_calls_buf,
                                                    reasoning.as_deref(),
                                                    std::mem::take(&mut thinking_blocks),
                                                );
                                        } else {
                                            // No tool calls — still preserve the
                                            // final-answer reasoning so the next
                                            // request echoes the REAL reasoning_content
                                            // (thinking-mode contract) instead of a
                                            // "(no reasoning recorded)" placeholder that
                                            // thinking models mimic back. See
                                            // `finalize_stream_with_reasoning`.
                                            conversation.finalize_stream_with_reasoning(
                                                reasoning.as_deref(),
                                                std::mem::take(&mut thinking_blocks),
                                            );
                                        }
                                        was_truncated = is_truncated;
                                        break;
                                    }

                                    Some(Ok(StreamEvent::Error(e))) => {
                                        let reasoning = reasoning_opt(&reasoning_buf);
                                        conversation.finalize_stream_with_reasoning(
                                            reasoning.as_deref(),
                                            std::mem::take(&mut thinking_blocks),
                                        );
                                        tel_return!(TurnResult::Failed(e), 0u32);
                                    }

                                    Some(Ok(StreamEvent::Warning(w))) => {
                                        // Advisory only — keep streaming. The
                                        // TUI surfaces this to the user so a
                                        // truncating proxy is visible at the
                                        // moment of the bad request, not three
                                        // hours later in the datalog.
                                        let _ = event_tx.send(TurnEvent::Warning(w));
                                    }

                                    Some(Err(e)) => {
                                        let reasoning = reasoning_opt(&reasoning_buf);
                                        conversation.finalize_stream_with_reasoning(
                                            reasoning.as_deref(),
                                            std::mem::take(&mut thinking_blocks),
                                        );
                                        tel_return!(TurnResult::Failed(e.to_string()), 0u32);
                                    }

                                    None => {
                                        // Stream ended without Done event — preserve any
                                        // captured reasoning, mirroring the Done branch.
                                        let reasoning = reasoning_opt(&reasoning_buf);
                                        conversation.finalize_stream_with_reasoning(
                                            reasoning.as_deref(),
                                            std::mem::take(&mut thinking_blocks),
                                        );
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
        let logged_reasoning = strip_reasoning_filler(&reasoning_buf);
        super::log::log_llm_response(
            &datalog_dir,
            pending_request_log,
            &text_buf,
            &tool_calls_buf,
            &logged_reasoning,
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

        // 5. If no tool calls, we're done — LLM produced text only.
        //    Use the FILTERED accumulator so downstream consumers
        //    (datalog `log_text`, ATLAS plan extraction, telemetry)
        //    see clean prose, not raw text_buf with leaked XML
        //    tool_call blocks. Earlier bug: 5-7 atomgr datalog
        //    20-14-23 Turn 5 logged `### 3. 传输层安全<tool_call>grep
        //    <arg_key>...` because Responded.text was raw.
        if tool_calls_buf.is_empty() {
            self.trigger_post_turn_hooks("Responded").await;
            tel_return!(
                TurnResult::Responded {
                    text: visible_text_buf,
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
            // Key on the *canonicalised* argument JSON so that semantically
            // identical calls with cosmetically different formatting collapse.
            // Weak/streaming models routinely re-emit the same call with
            // different whitespace, key order, or escape style:
            //   {"pattern":"foo"}   vs   {"pattern": "foo"}
            //   {"a":1,"b":2}       vs   {"b":2,"a":1}
            // The byte-identical comparison below would treat those as
            // distinct and let N ghost in-flight rows leak into the UI.
            // serde_json::to_string with a BTreeMap-backed Value sorts keys
            // and strips whitespace, so two formattings of the same object
            // yield the same canonical string. Non-JSON args fall back to
            // the raw string (no regression for free-form tools).
            let key = (call.name.clone(), normalize_tool_args(&call.arguments));
            if seen_calls.contains_key(&key) {
                is_dup[i] = true;
            } else {
                seen_calls.insert(key, i);
            }
        }

        // ── ToolBatchStarted: fires when ≥ 2 non-duplicate calls fan
        // out from one assistant message. Lets the UI render a single
        // grouped block instead of N independent ▸ rows.
        // Per-call ToolCallStarted events still fire below for backward
        // compat (UI dedupes via batch_id membership).
        let non_dup_count = is_dup.iter().filter(|d| !**d).count();
        let active_batch_id = if non_dup_count >= 2 {
            let batch_id = format!("batch_{}", uuid::Uuid::new_v4());
            let calls: Vec<crate::turn::event::ToolBatchCall> = tool_calls_buf
                .iter()
                .zip(is_dup.iter())
                .filter(|(_, dup)| !**dup)
                .map(|(c, _)| crate::turn::event::ToolBatchCall {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    arguments: c.arguments.clone(),
                })
                .collect();
            let _ = event_tx.send(TurnEvent::ToolBatchStarted {
                batch_id: batch_id.clone(),
                calls,
            });
            Some((batch_id, std::time::Instant::now(), non_dup_count))
        } else {
            None
        };
        let mut batch_ok_count: usize = 0;

        let mut files_edited_this_batch: Vec<String> = Vec::new();
        for (i, call) in tool_calls_buf.iter().enumerate() {
            if cancel.is_cancelled() {
                tel_return!(TurnResult::Cancelled, tool_count);
            }

            // ── Dup-in-batch: silent skip BEFORE any UI event ──
            // Some thinking-mode models emit the same tool_call N times in
            // one assistant message. Dispatching them all wastes execute
            // cycles, so we replay the first call's result for #2..N. The
            // model still sees one ToolResult per tool_call (parity
            // preserved via add_tool_result), but the UI must not render
            // ghost inflight rows for the duplicates — which it would if
            // ToolCallStarted fired before the is_dup gate.
            //
            // Symptom users saw before this gate moved up: a wall of
            // identical `Bash(...)` rows for each batch where the model
            // emitted N copies of the same call (e.g. dead_code grep
            // session with N variants pasted in by mistake).
            if is_dup[i] {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: "[Duplicate call — same tool and arguments as an earlier call in this batch. \
                             Result already returned above.]".to_string(),
                    success: true,
                };
                conversation.add_tool_result(result);
                continue;
            }

            // ── Cross-batch loop guard ──
            // The in-batch `is_dup` above only catches a model emitting
            // the same call N times *within one assistant message*. The
            // 22-identical-`Bash(cargo check)` symptom from weak models
            // is the orthogonal case: identical (name, args) repeating
            // across many sequential turns with no progress between.
            // See `loop_guard.rs` for the false-positive avoidance rules
            // (output-hash + state-change reset) that make this safe to
            // gate before execution. Same ghost-row reasoning as is_dup:
            // blocked attempts must not emit ToolCallStarted, otherwise
            // the UI renders a spinner row that never receives a result.
            if let LoopGuardDecision::Block(msg) =
                self.loop_guard.check(&call.name, &call.arguments)
            {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: msg,
                    // success=false so the model treats this as a soft
                    // error and is more likely to change strategy.
                    success: false,
                };
                conversation.add_tool_result(result);
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
            // Dup-in-batch was already short-circuited above (before the
            // ToolCallStarted emit), so by the time we reach here this is
            // a real, non-duplicate call to execute.
            let result = self
                .execute_single_tool(call, event_tx, &cancel, &conversation)
                .await;
            if active_batch_id.is_some() && result.success {
                batch_ok_count += 1;
            }

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

            // Record into the cross-batch loop guard. Must run on every
            // real execution (success OR failure) so the next turn's
            // check() sees the full history. The guard's own state-
            // change reset rule lives inside record() — runner doesn't
            // need to know the tool taxonomy.
            self.loop_guard
                .record(&call.name, &call.arguments, &result.output, result.success);

            conversation.add_tool_result(result);
        }

        // ── ToolBatchCompleted: closes the group started above. UI
        // uses this to swap the spinner header to a static `· N/M ok ·
        // Xs wall` summary. Only fires when a batch was actually opened.
        if let Some((batch_id, started_at, total)) = active_batch_id {
            let _ = event_tx.send(TurnEvent::ToolBatchCompleted {
                batch_id,
                ok: batch_ok_count,
                total,
                elapsed_ms: started_at.elapsed().as_millis() as u64,
            });
        }

        // Trigger post-turn hooks
        self.trigger_post_turn_hooks("UsedTools").await;

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
                // Same filtered-vs-raw split as the Responded arm above.
                // text_buf keeps raw for the rescue path; visible_text_buf
                // is what should reach downstream consumers.
                text: if visible_text_buf.is_empty() {
                    None
                } else {
                    Some(visible_text_buf)
                },
                tool_count,
                tokens: total_tokens,
            },
            tool_count
        );
    }

    /// Helper to trigger post-turn hooks with proper context
    async fn trigger_post_turn_hooks(&self, turn_result: &str) {
        let working_dir = self.context.working_dir.read().await.clone();
        let hook_ctx = HookCtx::new(
            "".to_string(),
            "".to_string(),
            working_dir.to_string_lossy().to_string(),
        )
        .with_session(self.provider.session_id())
        .with_turn(self.current_turn_number);
        self.hook_engine
            .trigger_post_turn(&hook_ctx, turn_result)
            .await;
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
        conversation: &crate::conversation::Conversation,
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
            } else if let Some(name) = self
                .tools
                .iter()
                .await
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
                let available: String = self
                    .tools
                    .iter()
                    .await
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
                self.context.telemetry.track(TelemetryEvent::ToolCall {
                    name: corrected_name.clone(),
                    success: false,
                    duration_ms: 0,
                    error_kind: Some(ToolErrorKind::NotFound),
                    error_data: Some(serde_json::json!({
                        "tool_name": call.name,
                        "duration_ms": 0,
                        "original_name": if call.name != corrected_name { Some(call.name.as_str()) } else { None },
                        "available_tools": available,
                        "reason": format!("Tool '{}' not found", call.name),
                    }).to_string()),
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

        // Schema gate: bounce malformed args back to the model BEFORE
        // approval / execute. Provider stream truncation occasionally
        // ships `{]` or `{"file_path":"..."]` (closing bracket wrong,
        // required field missing); without this guard, write_file's
        // fail-closed approval branch would prompt the user, the user
        // would Allow, and execute would then fail with the same parse
        // error — a wasted approval round-trip on a known-broken call.
        // Runs AFTER `repair_tool_args` (so wrapper-shape / fence / nested
        // payloads recover first) but BEFORE approval — the unrecoverable
        // remainder is what gets bounced.
        if let Err(reason) = tool.validate_args(&call.arguments) {
            // `reason` already comes from `diagnose_args` (or a sibling
            // helper) carrying a "Re-issue: <example>" tail. Appending
            // another "Re-issue {tool} with..." here produced the
            // double-Re-issue the user saw on the failed WriteFile call.
            let msg = format!("Error: {}", reason);
            let _ = event_tx.send(TurnEvent::ToolCallResult {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: msg.clone(),
                success: false,
                duration: std::time::Duration::ZERO,
            });
            self.context.telemetry.track(TelemetryEvent::ToolCall {
                name: corrected_name.clone(),
                success: false,
                duration_ms: 0,
                error_kind: Some(ToolErrorKind::InvalidArgs),
                error_data: Some(
                    serde_json::json!({
                        "tool_name": corrected_name,
                        "reason": reason,
                        "args_summary": build_args_summary(&corrected_name, &call.arguments),
                    })
                    .to_string(),
                ),
            });
            return ToolResult {
                call_id: call.id.clone(),
                output: msg,
                success: false,
            };
        }

        // Loop detection moved upstream to `dispatch_tools` (gates BEFORE
        // ToolCallStarted is emitted, so blocked attempts don't render
        // ghost inflight rows in scrollback). When we reach here the
        // call has already cleared that guard exactly once.

        // --- 统一 PreToolUse Hook (before approval — Issue #512) ---
        // PreToolUse hooks run BEFORE the approval check so that an explicit
        // "allow" from a hook can skip the ApprovalPrompt entirely.
        let working_dir = self.context.working_dir.read().await.clone();
        let pr_hook_ctx = HookCtx::new(
            call.name.clone(),
            call.arguments.clone(),
            working_dir.to_string_lossy().to_string(),
        );

        // --- 统一 ToolCallStart Hook (fire-and-forget) ---
        let tc_start_ctx = crate::hook::ToolCallStartContext {
            tool_name: call.name.clone(),
            tool_args: call.arguments.clone(),
            call_id: call.id.clone(),
            turn_number: self.current_turn_number,
        };
        self.hook_engine
            .trigger_on_tool_call_start(&tc_start_ctx)
            .await;

        let mut final_args = call.arguments.clone();
        // Deferred init: every non-diverging match arm below assigns this (the
        // `Err` arm returns early), so a `= false` initializer was dead.
        let hook_explicitly_allowed;
        match self.hook_engine.trigger_pre_tool_use(&pr_hook_ctx).await {
            Ok((Some(new_args), allowed)) => {
                final_args = new_args;
                hook_explicitly_allowed = allowed;
            }
            Ok((None, allowed)) => {
                hook_explicitly_allowed = allowed;
            }
            Err(reason) => {
                let output = format!("Tool '{}' was blocked by hook: {}", call.name, reason);
                let _ = event_tx.send(TurnEvent::ToolCallResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                    success: false,
                    duration: std::time::Duration::ZERO,
                });
                self.context.telemetry.track(TelemetryEvent::ToolCall {
                    name: corrected_name.clone(),
                    success: false,
                    duration_ms: 0,
                    error_kind: Some(ToolErrorKind::BlockedByHook),
                    error_data: Some(
                        serde_json::json!({
                            "tool_name": corrected_name,
                            "duration_ms": 0,
                            "args_summary": build_args_summary(&corrected_name, &call.arguments),
                            "hook_reason": reason,
                            "reason": "Tool call blocked by PreToolUse hook",
                        })
                        .to_string(),
                    ),
                });
                return ToolResult {
                    call_id: call.id.clone(),
                    output,
                    success: false,
                };
            }
        }

        // Check permission via the injected PermissionDecider.
        // AutoApprove tools execute immediately; RequireApproval tools go through
        // the decider which handles interactive prompts or automatic policy.
        // Skip approval entirely when a PreToolUse hook explicitly allowed
        // the tool call (Issue #512).
        if !hook_explicitly_allowed {
            let approval = tool.approval_with_context(&final_args, &self.context);
            if let crate::tool::ApprovalRequirement::RequireApproval(ref reason)
            | crate::tool::ApprovalRequirement::RequireApprovalAlways(ref reason)
            | crate::tool::ApprovalRequirement::RequireApprovalScoped { ref reason, .. } =
                approval
            {
                // Only emit the ApprovalRequested event (which triggers the
                // TUI approval prompt) when the decider actually needs user
                // input.  If the PermissionStore already has a session grant
                // or override (e.g. the user pressed [A] on a prior call of
                // the same tool in this batch), `will_auto_approve` returns
                // true and we skip the event — the subsequent `decide()` call
                // will return Allow without blocking.  Without this guard,
                // parallel MCP calls show N redundant "Waiting for approval"
                // prompts even though all but the first are auto-resolved.
                let needs_prompt = !self.permission.will_auto_approve(call, &approval);
                if needs_prompt {
                    // Emit an informational event carrying a snapshot of
                    // conversation state so the TUI can persist mid-turn
                    // session state (e.g. for `/bg`). Only clone here
                    // (lazily, when approval is actually needed) instead
                    // of per-tool-call — avoids O(N) full message-list
                    // clones for batch turns with many tools.
                    let _ = event_tx.send(TurnEvent::ApprovalRequested {
                        tool_name: call.name.clone(),
                        reason: reason.clone(),
                        call: call.clone(),
                        snapshot: conversation.snapshot(),
                    });
                }

                let decision = self.permission.decide(call, &approval).await;
                if !matches!(decision, PermissionDecision::Allow) {
                    let output = format!("Tool '{}' was denied by the user.", call.name);
                    let _ = event_tx.send(TurnEvent::ToolCallResult {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        output: output.clone(),
                        success: false,
                        duration: std::time::Duration::ZERO,
                    });
                    self.context.telemetry.track(TelemetryEvent::ToolCall {
                    name: corrected_name.clone(),
                    success: false,
                    duration_ms: 0,
                    error_kind: Some(ToolErrorKind::DeniedByUser),
                    error_data: Some(serde_json::json!({
                        "tool_name": corrected_name,
                        "duration_ms": 0,
                        "args_summary": build_args_summary(&corrected_name, &call.arguments),
                        "approval_reason": reason,
                        "reason": "User denied tool execution",
                    }).to_string()),
                });
                    return ToolResult {
                        call_id: call.id.clone(),
                        output,
                        success: false,
                    };
                }
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
        crate::ctrace!(
            "RNR",
            "execute_single_tool start name={} cancel_already={}",
            call.name,
            cancel.is_cancelled()
        );
        let result = tokio::select! {
            r = tool.execute(&call.arguments, &self.context) => {
                crate::ctrace!("RNR", "execute_single_tool tool returned name={} elapsed_ms={} cancel_now={}", call.name, start.elapsed().as_millis(), cancel.is_cancelled());
                r
            },
            _ = cancel.cancelled() => {
                crate::ctrace!("RNR", "execute_single_tool cancel branch fired name={} elapsed_ms={}", call.name, start.elapsed().as_millis());
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
                self.context.telemetry.track(TelemetryEvent::ToolCall {
                    name: corrected_name.clone(),
                    success: false,
                    duration_ms: duration.as_millis() as u32,
                    error_kind: Some(ToolErrorKind::ExecutionFailed),
                    error_data: Some(serde_json::json!({
                        "tool_name": corrected_name,
                        "duration_ms": duration.as_millis() as u32,
                        "args_summary": build_args_summary(&corrected_name, &call.arguments),
                        "output_tail": "[Cancelled by user]",
                        "reason": "Tool execution cancelled by user",
                    }).to_string()),
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

        // --- 统一 PostToolUse Hook ---
        let result_ctx = ToolResultContext {
            tool_name: call.name.clone(),
            tool_args: final_args.clone(),
            result: tool_result.output.clone(),
            success: tool_result.success,
            duration_ms: duration.as_millis() as u64,
        };
        self.hook_engine
            .trigger_post_tool_use(&pr_hook_ctx, &result_ctx)
            .await;

        let _ = event_tx.send(TurnEvent::ToolCallResult {
            call_id: call.id.clone(),
            name: call.name.clone(),
            output: tool_result.output.clone(),
            success: tool_result.success,
            duration,
        });

        // Emit ToolCall telemetry event for both success and failure.
        let scrub_home = crate::tool::real_home_dir();
        let output_tail = atomcode_telemetry::scrub::truncate_head(
            &atomcode_telemetry::scrub::scrub_path(
                &tool_result.output,
                scrub_home.as_deref(),
                Some(&self.context.working_dir.read().await.clone()),
            ),
            200,
        );
        // Detect warning: exit 0 (success) but stderr present.
        let has_stderr =
            tool_result.output.contains("STDERR:") || tool_result.output.contains("[stderr]");
        let (error_kind, error_data) = if !tool_result.success {
            (
                Some(ToolErrorKind::ExecutionFailed),
                Some(
                    serde_json::json!({
                        "tool_name": corrected_name,
                        "duration_ms": duration.as_millis() as u32,
                        "args_summary": build_args_summary(&corrected_name, &call.arguments),
                        "output_tail": output_tail,
                        "reason": "Tool execution returned an error",
                    })
                    .to_string(),
                ),
            )
        } else if has_stderr {
            (Some(ToolErrorKind::Warning), Some(serde_json::json!({
                "tool_name": corrected_name,
                "duration_ms": duration.as_millis() as u32,
                "args_summary": build_args_summary(&corrected_name, &call.arguments),
                "output_tail": output_tail,
                "reason": "Command succeeded (exit 0) but produced stderr output",
                "resolution": "Review stderr for potential issues; the command may not have had the intended effect",
            }).to_string()))
        } else {
            (None, None)
        };
        self.context.telemetry.track(TelemetryEvent::ToolCall {
            name: corrected_name.clone(),
            success: tool_result.success,
            duration_ms: duration.as_millis() as u32,
            error_kind,
            error_data,
        });

        tool_result
    }
}

/// Canonicalise a tool-call `arguments` string for in-batch dedup keying.
///
/// Weak/streaming models routinely re-emit the same call with cosmetically
/// different formatting — `{"pattern":"foo"}` vs `{"pattern": "foo"}` vs
/// `{"a":1,"b":2}` vs `{"b":2,"a":1}`. Byte-comparison treats them as
/// distinct, the in-batch `is_dup` misses, and N ghost ToolCallInFlight
/// rows leak into the UI (the symptom from the deepseek-v4-flash
/// screenshot: 2 empty `Glob(**/*.rs)` rows + 1 with body).
///
/// We re-parse and serialise compact. `serde_json::Map` is BTreeMap-backed
/// when the `preserve_order` feature is off (it is — see workspace
/// Cargo.toml), so object keys come out alphabetically — two re-orderings
/// of the same object hash to the same canonical string. Non-JSON args
/// (free-form text, garbage from broken streams) round-trip through the
/// fallback unchanged so we don't regress free-form tools or accidentally
/// merge two genuinely different malformed payloads.
/// True iff `reasoning` contains nothing besides known bogus reasoning fillers
/// interleaved with whitespace — including the all-empty / all-whitespace case.
///
/// The Done-event skip-promotion guard uses this to detect not just
/// the trivial single-copy echo but also the multi-copy mimicry seen
/// on DeepSeek V4 thinking-mode (a long session has many historical
/// copies of the placeholder in context, and the model regenerates the
/// pattern in its own response — observed 3+ copies concatenated in a
/// single reasoning_content stream). It also covers legacy/upstream protocol
/// residue such as `(no reasoning detected)</｜｜DSML｜｜parameter>`.
fn is_only_placeholder_filler(reasoning: &str) -> bool {
    strip_reasoning_filler(reasoning).trim().is_empty()
}

// KEEP IN SYNC WITH crates/atomcode-kernel/src/agent.rs `strip_reasoning_filler`
// (and its `strip_dsml_parameter_fragments` / `strip_leading_parameter_tail` helpers).
// These are intentionally duplicated because kernel takes no dependency on core;
// any bugfix here must be applied to the kernel copy as well.
fn strip_reasoning_filler(reasoning: &str) -> String {
    const LEGACY_REASONING_FILLERS: &[&str] = &[
        crate::provider::REASONING_PLACEHOLDER,
        "(no reasoning detected)",
        "(no reasoning recorded)",
        "no reasoning detected",
        "no reasoning recorded",
    ];

    let (mut cleaned, mut changed) = strip_dsml_parameter_fragments(reasoning);
    for marker in LEGACY_REASONING_FILLERS {
        if cleaned.contains(marker) {
            cleaned = cleaned.replace(marker, "");
            changed = true;
        }
    }

    if changed {
        let (tail_cleaned, tail_changed) = strip_leading_parameter_tail(&cleaned);
        if tail_changed {
            cleaned = tail_cleaned;
        }
    }

    if changed {
        cleaned.trim().to_string()
    } else {
        cleaned
    }
}

fn strip_dsml_parameter_fragments(input: &str) -> (String, bool) {
    let mut rest = input;
    let mut out = String::new();
    let mut changed = false;

    while let Some(dsml_idx) = rest.find("DSML") {
        let before_dsml = &rest[..dsml_idx];
        let after_dsml = &rest[dsml_idx..];
        let start = before_dsml.rfind('<');
        let end = after_dsml.find('>');

        if let (Some(start), Some(end)) = (start, end) {
            let end = dsml_idx + end + 1;
            let fragment = &rest[start..end];
            if fragment.to_ascii_lowercase().contains("parameter") {
                out.push_str(&rest[..start]);
                rest = &rest[end..];
                changed = true;
                continue;
            }
        }

        let split = dsml_idx + "DSML".len();
        out.push_str(&rest[..split]);
        rest = &rest[split..];
    }

    out.push_str(rest);
    (out, changed)
}

/// Strip a legacy tail left behind after removing `(no reasoning detected)`.
/// Keep this deliberately narrow so real XML/code examples using
/// `<parameter ...>` survive unchanged.
fn strip_leading_parameter_tail(input: &str) -> (String, bool) {
    let trimmed = input.trim_start();
    if !trimmed
        .get(.."</parameter>".len())
        .is_some_and(|tag| tag.eq_ignore_ascii_case("</parameter>"))
    {
        return (input.to_string(), false);
    }

    let skipped_ws = input.len() - trimmed.len();
    let tail_start = skipped_ws + "</parameter>".len();
    (input[tail_start..].to_string(), true)
}

/// `None` for an empty/whitespace/filler-only reasoning buffer, else the
/// reasoning with known bogus fillers removed.
///
/// Used at every stream-termination site (Done, error, abort, stream-end) so
/// any captured thinking-model reasoning is preserved into history rather than
/// dropped — `finalize_stream_with_reasoning(None, _)` is identical to the old
/// `finalize_stream()`, so non-thinking models are unaffected. Preserving real
/// reasoning means the next request echoes it instead of falling back to the
/// `REASONING_PLACEHOLDER` filler.
fn reasoning_opt(buf: &str) -> Option<String> {
    let cleaned = strip_reasoning_filler(buf);
    if cleaned.trim().is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn normalize_tool_args(args: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => serde_json::to_string(&v).unwrap_or_else(|_| args.to_string()),
        Err(_) => args.to_string(),
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
    name.chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '=' | '<' | '>'))
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

/// Rescue tool calls embedded as text in the model's response. Four variants:
///   1. `<tool_call>name(json)</tool_call>` — paren+JSON
///   2. `<tool_call>name(k=v, k=v)</tool_call>` — paren+kv (legacy single-line)
///   3. `<tool_call><tool_name>name</tool_name><arg_key>k</arg_key><arg_value>v</arg_value>...</tool_call>`
///      — Qwen2.5 / GLM XML format
///   4. `<tool_call><function=name><parameter=k>v</parameter>...</function></tool_call>`
///      — OpenHands / Qwen3+ "function-tag" format (attribute-style)
/// Returns rescued ToolCalls, empty vec if nothing found.
fn rescue_text_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    let mut remaining = text;

    while let Some(start) = remaining.find("<tool_call>") {
        let after_tag = &remaining[start + "<tool_call>".len()..];

        // Prefer </tool_call> close (XML format spans newlines).
        // Fall back to first newline only when no close tag is present
        // (legacy single-line format).
        let (body, advance) = match after_tag.find("</tool_call>") {
            Some(pos) => (&after_tag[..pos], pos + "</tool_call>".len()),
            None => {
                let pos = after_tag.find('\n').unwrap_or(after_tag.len());
                (&after_tag[..pos], pos)
            }
        };
        let body = body.trim();

        if let Some((name, args_json)) =
            parse_xml_tool_call(body).or_else(|| parse_xml_attr_style_tool_call(body))
        {
            let call_id = format!("rescued_{}", calls.len());
            calls.push(ToolCall {
                id: call_id,
                name,
                arguments: args_json,
            });
        } else if let Some(paren) = body.find('(') {
            let name = body[..paren].trim();
            let args_raw = body[paren + 1..].trim_end_matches(')').trim();

            if !name.is_empty() {
                let args_json = if args_raw.starts_with('{') {
                    args_raw.to_string()
                } else {
                    let mut json_parts = Vec::new();
                    for part in args_raw.split(',') {
                        let part = part.trim();
                        if let Some(eq) = part.find('=') {
                            let k = part[..eq].trim();
                            let v = part[eq + 1..].trim();
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

        remaining = &after_tag[advance..];
    }

    calls
}

/// Parse Qwen2.5 / GLM XML-style tool call body:
///   `<tool_name>NAME</tool_name><arg_key>K1</arg_key><arg_value>V1</arg_value>...`
/// Returns `(name, args_as_json_object)` or None when the format doesn't match.
fn parse_xml_tool_call(body: &str) -> Option<(String, String)> {
    let name = extract_between(body, "<tool_name>", "</tool_name>")?
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }

    let mut map = serde_json::Map::new();
    let mut rest = body;
    while let Some(k_start) = rest.find("<arg_key>") {
        let k_after = &rest[k_start + "<arg_key>".len()..];
        let k_end = k_after.find("</arg_key>")?;
        let key = k_after[..k_end].trim().to_string();
        if key.is_empty() {
            return None;
        }
        let after_key = &k_after[k_end + "</arg_key>".len()..];
        let v_start = after_key.find("<arg_value>")?;
        let v_after = &after_key[v_start + "<arg_value>".len()..];
        let v_end = v_after.find("</arg_value>")?;
        let raw_value = &v_after[..v_end];
        map.insert(key, coerce_xml_value(raw_value));
        rest = &v_after[v_end + "</arg_value>".len()..];
    }

    if map.is_empty() {
        return None;
    }
    Some((name, serde_json::Value::Object(map).to_string()))
}

/// Parse OpenHands / Qwen3+ "function-tag" XML-style tool call body:
///   `<function=NAME><parameter=K1>V1</parameter><parameter=K2>V2</parameter></function>`
/// Returns `(name, args_as_json_object)` or None when the format doesn't match.
///
/// Differs from `parse_xml_tool_call` in two ways:
///   * the function name rides on the opening tag's attribute slot
///     (`<function=read_file>`), not inside a `<tool_name>` wrapper
///   * each parameter is one `<parameter=KEY>VALUE</parameter>` element,
///     not two separate `<arg_key>` / `<arg_value>` pairs
///
/// This format is what Qwen3 / Qwen3-Coder / Qwen3.6 / GLM-Z1-Agent emit
/// when the inference gateway doesn't parse them out into the OpenAI
/// `tool_calls` field — they were SFT-trained on OpenHands-style data.
/// Without this rescue, the XML leaks into the visible assistant text
/// and the tool never executes.
fn parse_xml_attr_style_tool_call(body: &str) -> Option<(String, String)> {
    let fn_marker = body.find("<function=")?;
    let after_marker = &body[fn_marker + "<function=".len()..];
    let close_bracket = after_marker.find('>')?;
    let name = after_marker[..close_bracket].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let inner_start = close_bracket + 1;
    let inner_end = after_marker[inner_start..].find("</function>")? + inner_start;
    let inner = &after_marker[inner_start..inner_end];

    let mut map = serde_json::Map::new();
    let mut rest = inner;
    while let Some(p_marker) = rest.find("<parameter=") {
        let after_p = &rest[p_marker + "<parameter=".len()..];
        let cb = after_p.find('>')?;
        let key = after_p[..cb].trim().to_string();
        if key.is_empty() {
            return None;
        }
        let val_start = cb + 1;
        let val_end = after_p[val_start..].find("</parameter>")? + val_start;
        let raw = &after_p[val_start..val_end];
        // Function-tag format puts every value on its own line(s); strip a
        // single leading and trailing newline that's there purely for layout.
        // Don't `.trim()` — `old_string` for edit_file may legitimately need
        // its internal whitespace preserved for the file-match to land.
        let value = raw.strip_prefix('\n').unwrap_or(raw);
        let value = value.strip_suffix('\n').unwrap_or(value);
        map.insert(key, coerce_xml_value(value));
        rest = &after_p[val_end + "</parameter>".len()..];
    }

    if map.is_empty() {
        return None;
    }
    Some((name, serde_json::Value::Object(map).to_string()))
}

fn extract_between<'a>(haystack: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let s = haystack.find(open)? + open.len();
    let e = haystack[s..].find(close)? + s;
    Some(&haystack[s..e])
}

/// Best-effort type inference for `<arg_value>` payloads. Bool/int/float/JSON
/// literals get unquoted; everything else stays a string (preserves whitespace).
fn coerce_xml_value(raw: &str) -> serde_json::Value {
    let trimmed = raw.trim();
    if trimmed == "true" {
        return serde_json::Value::Bool(true);
    }
    if trimmed == "false" {
        return serde_json::Value::Bool(false);
    }
    if trimmed == "null" {
        return serde_json::Value::Null;
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        return serde_json::Value::from(n);
    }
    if let Ok(f) = trimmed.parse::<f64>() {
        return serde_json::Value::from(f);
    }
    if (trimmed.starts_with('{') && trimmed.ends_with('}'))
        || (trimmed.starts_with('[') && trimmed.ends_with(']'))
    {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return v;
        }
    }
    // Preserve raw (including leading/trailing whitespace) — the model may have
    // intended exact-match strings (e.g. old_string for edit_file).
    serde_json::Value::String(raw.to_string())
}

/// Patch `tool_calls_buf` entries with missing keys borrowed from `xml_pool`
/// (parsed by `rescue_text_tool_calls` on the same turn's raw text). Used when
/// the model split intent across the function-calling JSON channel and the
/// `<tool_call>` XML in the text — the JSON path may arrive with only a subset
/// of the arguments, while the XML carries the full set. JSON wins on
/// conflicts; XML only fills gaps. Multiple calls of the same name are matched
/// to XML blocks of the same name in order of appearance.
fn repair_tool_call_args(calls: &mut [ToolCall], xml_pool: &[ToolCall]) {
    use std::collections::HashMap;

    let mut by_name: HashMap<&str, Vec<&ToolCall>> = HashMap::new();
    for x in xml_pool {
        by_name.entry(x.name.as_str()).or_default().push(x);
    }
    let mut consumed: HashMap<&str, usize> = HashMap::new();

    for call in calls.iter_mut() {
        let Some(group) = by_name.get(call.name.as_str()) else {
            continue;
        };
        let idx = consumed.entry(call.name.as_str()).or_insert(0);
        let Some(xml_call) = group.get(*idx) else {
            continue;
        };
        *idx += 1;

        let xml_obj = match serde_json::from_str::<serde_json::Value>(&xml_call.arguments) {
            Ok(serde_json::Value::Object(o)) => o,
            _ => continue,
        };

        let merged = match serde_json::from_str::<serde_json::Value>(&call.arguments) {
            Ok(serde_json::Value::Object(mut j_obj)) => {
                let mut patched = false;
                for (k, v) in xml_obj {
                    if !j_obj.contains_key(&k) {
                        j_obj.insert(k, v);
                        patched = true;
                    }
                }
                if patched {
                    Some(serde_json::Value::Object(j_obj))
                } else {
                    None
                }
            }
            // JSON args unparseable / non-object → take XML wholesale.
            _ => Some(serde_json::Value::Object(xml_obj)),
        };
        if let Some(v) = merged {
            call.arguments = v.to_string();
        }
    }
}

/// Streaming filter that hides `<tool_call>...</tool_call>` blocks from the
/// visible UI/conversation stream while letting the rescue path see the full
/// raw text via a separate buffer. Tags can split across delta chunks, so the
/// filter holds back trailing bytes that might be a partial tag.
#[derive(Default)]
struct ToolCallStreamFilter {
    inside: bool,
    holdback: String,
}

impl ToolCallStreamFilter {
    const OPEN: &'static str = "<tool_call>";
    const CLOSE: &'static str = "</tool_call>";

    /// Feed a delta chunk; return what's safe to display now.
    fn feed(&mut self, chunk: &str) -> String {
        let mut work = std::mem::take(&mut self.holdback);
        work.push_str(chunk);
        let mut out = String::new();

        loop {
            if self.inside {
                match work.find(Self::CLOSE) {
                    Some(pos) => {
                        work = work[pos + Self::CLOSE.len()..].to_string();
                        self.inside = false;
                    }
                    None => {
                        self.holdback = trail_holdback(&work, Self::CLOSE.len() - 1);
                        return out;
                    }
                }
            } else {
                match work.find(Self::OPEN) {
                    Some(pos) => {
                        out.push_str(&work[..pos]);
                        work = work[pos + Self::OPEN.len()..].to_string();
                        self.inside = true;
                    }
                    None => {
                        let hold = trail_holdback(&work, Self::OPEN.len() - 1);
                        let visible_len = work.len() - hold.len();
                        out.push_str(&work[..visible_len]);
                        self.holdback = hold;
                        return out;
                    }
                }
            }
        }
    }

    /// End-of-stream flush. If we're still inside an unclosed `<tool_call>`,
    /// the holdback is dropped (prevents leak); otherwise emit any held tail.
    fn flush(&mut self) -> String {
        if self.inside {
            self.holdback.clear();
            String::new()
        } else {
            std::mem::take(&mut self.holdback)
        }
    }
}

/// Take up to `max` trailing bytes from `s`, snapped down to a UTF-8 char
/// boundary so the holdback is always a valid `String`.
fn trail_holdback(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut split = s.len() - max;
    while split < s.len() && !s.is_char_boundary(split) {
        split += 1;
    }
    s[split..].to_string()
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
mod is_only_placeholder_filler_tests {
    use super::{is_only_placeholder_filler, strip_reasoning_filler};
    use crate::provider::REASONING_PLACEHOLDER;

    #[test]
    fn empty_and_whitespace_are_filler() {
        assert!(is_only_placeholder_filler(""));
        assert!(is_only_placeholder_filler("   "));
        assert!(is_only_placeholder_filler("\n\t  \n"));
    }

    #[test]
    fn single_placeholder_is_filler() {
        // The original strict-equality guard already caught this; pin
        // it so a refactor doesn't regress.
        assert!(is_only_placeholder_filler(REASONING_PLACEHOLDER));
    }

    #[test]
    fn multiple_concatenated_placeholders_are_filler() {
        // The bug: DeepSeek V4 Flash 17-round session screenshot
        // showed the response's reasoning_content as 3 copies of the
        // placeholder concatenated with no separator. The old
        // `!= REASONING_PLACEHOLDER` check missed this and promoted
        // the meaningless string into the assistant text channel.
        let three = REASONING_PLACEHOLDER.repeat(3);
        assert!(is_only_placeholder_filler(&three));
        let five = REASONING_PLACEHOLDER.repeat(5);
        assert!(is_only_placeholder_filler(&five));
    }

    #[test]
    fn placeholders_with_whitespace_are_filler() {
        // Some gateways insert chunk delimiters (newlines, spaces)
        // between repeated placeholder echoes. Filler regardless.
        let mixed = format!(
            "{}\n{}  {}",
            REASONING_PLACEHOLDER, REASONING_PLACEHOLDER, REASONING_PLACEHOLDER
        );
        assert!(is_only_placeholder_filler(&mixed));
    }

    #[test]
    fn legacy_no_reasoning_detected_with_dsml_tail_is_filler() {
        let legacy = "(no reasoning detected)</｜｜DSML｜｜parameter>";
        assert!(is_only_placeholder_filler(legacy));
    }

    #[test]
    fn legacy_no_reasoning_recorded_with_ascii_dsml_tail_is_filler() {
        let legacy = "(no reasoning recorded)</|||DSML|||parameter>";
        assert!(is_only_placeholder_filler(legacy));
    }

    #[test]
    fn real_reasoning_is_not_filler() {
        assert!(!is_only_placeholder_filler(
            "Let me think about this — first, the user wants..."
        ));
    }

    #[test]
    fn placeholder_plus_real_content_is_not_filler() {
        // If the model emits the placeholder AND some substantive
        // text, we still want promotion — the substantive text is
        // the real reasoning we'd want to keep.
        let mixed = format!("{} but actually I see now that...", REASONING_PLACEHOLDER);
        assert!(!is_only_placeholder_filler(&mixed));
    }

    #[test]
    fn strip_reasoning_filler_removes_legacy_markers_but_keeps_real_content() {
        let mixed = "(no reasoning detected)</｜｜DSML｜｜parameter> actual answer";
        assert_eq!(strip_reasoning_filler(mixed), "actual answer");
    }

    #[test]
    fn strip_reasoning_filler_preserves_real_parameter_xml_without_legacy_marker() {
        let real = r#"Use <parameter name="path">src/main.rs</parameter> in the request."#;
        assert_eq!(strip_reasoning_filler(real), real);
    }

    #[test]
    fn reasoning_opt_maps_empty_to_none_and_content_to_some_verbatim() {
        use super::reasoning_opt;
        assert_eq!(reasoning_opt(""), None);
        assert_eq!(reasoning_opt("   \n\t "), None);
        assert_eq!(
            reasoning_opt("real thinking").as_deref(),
            Some("real thinking")
        );
        // Emptiness is judged after trim, but the value is returned verbatim
        // (never trimmed) so we never mutate captured reasoning.
        assert_eq!(reasoning_opt("  pad  ").as_deref(), Some("  pad  "));
    }
}

#[cfg(test)]
mod normalize_tool_args_tests {
    use super::normalize_tool_args;

    #[test]
    fn whitespace_variants_collapse() {
        // The deepseek-v4-flash screenshot symptom: same call, different
        // whitespace → must dedup.
        let a = r#"{"pattern":"**/*.rs"}"#;
        let b = r#"{"pattern": "**/*.rs"}"#;
        let c = r#"{ "pattern":"**/*.rs" }"#;
        let d = r#"{
  "pattern": "**/*.rs"
}"#;
        let na = normalize_tool_args(a);
        assert_eq!(normalize_tool_args(b), na);
        assert_eq!(normalize_tool_args(c), na);
        assert_eq!(normalize_tool_args(d), na);
    }

    #[test]
    fn key_order_collapses() {
        // serde_json::Map is BTreeMap-backed (no preserve_order feature),
        // so re-serialising sorts keys alphabetically.
        let a = r#"{"a":1,"b":2}"#;
        let b = r#"{"b":2,"a":1}"#;
        assert_eq!(normalize_tool_args(a), normalize_tool_args(b));
    }

    #[test]
    fn nested_objects_normalize_recursively() {
        let a = r#"{"outer":{"x":1,"y":2}}"#;
        let b = r#"{"outer":{"y":2,"x":1}}"#;
        assert_eq!(normalize_tool_args(a), normalize_tool_args(b));
    }

    #[test]
    fn semantically_different_args_stay_different() {
        // Don't over-collapse — different values must remain distinct so a
        // legitimate batch of `Glob(**/*.rs)` + `Glob(**/*.toml)` doesn't
        // dedup.
        let a = r#"{"pattern":"**/*.rs"}"#;
        let b = r#"{"pattern":"**/*.toml"}"#;
        assert_ne!(normalize_tool_args(a), normalize_tool_args(b));
    }

    #[test]
    fn non_json_args_pass_through_unchanged() {
        // Free-form / malformed payloads must not panic or merge.
        // (Two genuinely different garbage strings must stay distinct so
        // we don't accidentally dedup unrelated calls.)
        let raw = "not even json {{{";
        assert_eq!(normalize_tool_args(raw), raw);
        assert_ne!(
            normalize_tool_args("garbage A"),
            normalize_tool_args("garbage B")
        );
    }
}

#[cfg(test)]
mod tool_call_text_rescue_tests {
    use super::{repair_tool_call_args, rescue_text_tool_calls, ToolCall, ToolCallStreamFilter};

    #[test]
    fn rescues_qwen_xml_format() {
        // Qwen/GLM-5.1 sometimes emits args as <arg_key>/<arg_value> XML pairs
        // instead of a JSON blob. Without parsing this format the call gets
        // dispatched with empty args and edit_file fails with "old_string is
        // required" while the raw XML leaks into the user-visible stream.
        let text = r#"Let me make the edit:
<tool_call>
  <tool_name>edit_file</tool_name>
  <arg_key>file_path</arg_key><arg_value>src/main.rs</arg_value>
  <arg_key>old_string</arg_key><arg_value>        attrs
      }
  }</arg_value>
  <arg_key>new_string</arg_key><arg_value>        attrs.push(x);
        attrs
      }
  }</arg_value>
  <arg_key>replace_all</arg_key><arg_value>false</arg_value>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "single XML block should rescue one call");
        assert_eq!(calls[0].name, "edit_file");
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["file_path"], "src/main.rs");
        assert_eq!(v["replace_all"], false);
        // Whitespace-sensitive — old_string must round-trip exactly so edit_file
        // can find the match in the file.
        assert_eq!(v["old_string"], "        attrs\n      }\n  }");
    }

    #[test]
    fn xml_without_tool_name_is_skipped() {
        // No <tool_name> means we have no idea what to dispatch — better to
        // skip than guess. An XML block with only <arg_key> tags is treated as
        // a malformed legacy emit (no `(` either, so the paren branch also
        // skips), yielding zero calls.
        let text = r#"<tool_call>
  <arg_key>file_path</arg_key><arg_value>x.rs</arg_value>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert!(calls.is_empty());
    }

    #[test]
    fn legacy_paren_json_format_still_works() {
        // Don't regress the existing rescue path used by GLM-5 via OpenRouter.
        let text = r#"<tool_call>read_file({"file_path":"a.rs"})</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["file_path"], "a.rs");
    }

    #[test]
    fn legacy_paren_kv_format_still_works() {
        let text = r#"<tool_call>read_file(file_path=a.rs, offset=10)</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["file_path"], "a.rs");
        assert_eq!(v["offset"], 10);
    }

    #[test]
    fn xml_coerces_bool_int_float() {
        let text = r#"<tool_call>
  <tool_name>cfg</tool_name>
  <arg_key>flag</arg_key><arg_value>true</arg_value>
  <arg_key>n</arg_key><arg_value>42</arg_value>
  <arg_key>f</arg_key><arg_value>3.14</arg_value>
  <arg_key>s</arg_key><arg_value>hello</arg_value>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["flag"], true);
        assert_eq!(v["n"], 42);
        assert!((v["f"].as_f64().unwrap() - 3.14).abs() < 1e-9);
        assert_eq!(v["s"], "hello");
    }

    #[test]
    fn rescues_qwen3_function_tag_format() {
        // Qwen3 / Qwen3-Coder / Qwen3.6 (and other OpenHands-trained agents)
        // emit tool calls as `<function=NAME><parameter=K>V</parameter>...
        // </function>`. Mirrors the exact shape captured in the 21:17 Qwen3.6
        // screenshot: two sequential read_file calls with int + path params.
        let text = r#"Let me look at the exact code structure more carefully.

<tool_call>
<function=read_file>
<parameter=limit>
30
</parameter>
<parameter=offset>
147
</parameter>
<parameter=file_path>
/tmp/cc-switch-src/src-tauri/src/proxy/response_processor.rs
</parameter>
</function>
</tool_call>
<tool_call>
<function=read_file>
<parameter=limit>
20
</parameter>
<parameter=offset>
530
</parameter>
<parameter=file_path>
/tmp/cc-switch-src/src-tauri/src/proxy/providers/streaming.rs
</parameter>
</function>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert_eq!(
            calls.len(),
            2,
            "two sequential blocks must rescue two calls"
        );

        let v0: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(v0["limit"], 30);
        assert_eq!(v0["offset"], 147);
        // File path must NOT carry the layout newlines that wrapped the value
        // inside the `<parameter>` element — file dispatch on a `"\n/tmp/...\n"`
        // arg would 404.
        assert_eq!(
            v0["file_path"],
            "/tmp/cc-switch-src/src-tauri/src/proxy/response_processor.rs"
        );

        let v1: serde_json::Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(calls[1].name, "read_file");
        assert_eq!(v1["limit"], 20);
        assert_eq!(v1["offset"], 530);
        assert_eq!(
            v1["file_path"],
            "/tmp/cc-switch-src/src-tauri/src/proxy/providers/streaming.rs"
        );
    }

    #[test]
    fn function_tag_preserves_internal_whitespace_for_edit_file_old_string() {
        // edit_file's `old_string` is matched against the file verbatim, so
        // internal whitespace MUST round-trip. The layout-newline strip should
        // only peel ONE leading and ONE trailing `\n`, leaving multi-line
        // bodies intact.
        let text = r#"<tool_call>
<function=edit_file>
<parameter=file_path>
src/main.rs
</parameter>
<parameter=old_string>
        attrs
      }
  }
</parameter>
<parameter=new_string>
        attrs.push(x);
        attrs
      }
  }
</parameter>
</function>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "edit_file");
        let v: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(v["file_path"], "src/main.rs");
        assert_eq!(v["old_string"], "        attrs\n      }\n  }");
        assert_eq!(
            v["new_string"],
            "        attrs.push(x);\n        attrs\n      }\n  }"
        );
    }

    #[test]
    fn function_tag_without_close_function_tag_skips() {
        // Missing `</function>` close — better to drop the rescue than guess
        // where the call body ended. Matches the existing safety posture in
        // `parse_xml_tool_call` (returns None on missing tags rather than
        // making things up).
        let text = r#"<tool_call>
<function=read_file>
<parameter=file_path>x.rs</parameter>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert!(
            calls.is_empty(),
            "missing </function> must yield zero calls"
        );
    }

    #[test]
    fn function_tag_without_any_parameter_skips() {
        // A `<function=...>` with no `<parameter=...>` children means we'd
        // dispatch with empty args — usually broken intent. Skip rather than
        // guess (mirrors `xml_without_tool_name_is_skipped`).
        let text = r#"<tool_call>
<function=list_files>
</function>
</tool_call>"#;
        let calls = rescue_text_tool_calls(text);
        assert!(calls.is_empty(), "no <parameter> children → no rescue");
    }

    #[test]
    fn stream_filter_passes_plain_text() {
        let mut f = ToolCallStreamFilter::default();
        let out = f.feed("hello world");
        // Holdback may keep up to 10 bytes in case "<tool_call" is starting,
        // so flush to get the full output.
        let tail = f.flush();
        assert_eq!(format!("{}{}", out, tail), "hello world");
    }

    /// Regression for 5-7 atomgr datalog (build dd425fd, 20-14-23 Turn 5):
    /// GLM-5.1 emitted prose then mid-sentence switched to XML tool_call:
    /// `### 3. 传输层安全<tool_call>grep<arg_key>pattern</arg_key>...
    /// </tool_call>`. The stream_filter caught it for streamed deltas /
    /// conversation history, but `TurnResult::Responded.text` used raw
    /// `text_buf` → `datalog::log_text` printed the XML in `**Response:**`.
    ///
    /// Fix: parallel `visible_text_buf` mirrors what the filter actually
    /// emitted; `Responded.text` and `UsedTools.text` use it instead of
    /// raw text_buf. This test pins the visible-side behavior for the
    /// exact Turn 5 input shape.
    #[test]
    fn glm_xml_leak_mid_prose_strips_to_clean_visible_text() {
        let mut f = ToolCallStreamFilter::default();
        let mut visible = String::new();

        // Replay the actual Turn 5 chunking shape: prose, then XML
        // tool_call split across multiple deltas (provider chunks at
        // arbitrary boundaries — the filter must hold back across them).
        for chunk in [
            "### 3. 传输层安全",
            "<tool_call>grep",
            "<arg_key>pattern</arg_key>",
            "<arg_value>http://</arg_value>",
            "<arg_key>path</arg_key>",
            "<arg_value>/Users/y/project</arg_value>",
            "</tool_call>",
        ] {
            visible.push_str(&f.feed(chunk));
        }
        visible.push_str(&f.flush());

        assert!(
            !visible.contains("<tool_call>"),
            "visible accumulator must strip <tool_call> open tag: {:?}",
            visible
        );
        assert!(
            !visible.contains("</tool_call>"),
            "visible accumulator must strip </tool_call> close tag: {:?}",
            visible
        );
        assert!(
            !visible.contains("<arg_key>") && !visible.contains("<arg_value>"),
            "visible accumulator must strip XML inner tags: {:?}",
            visible
        );
        assert_eq!(
            visible, "### 3. 传输层安全",
            "only the pre-tool prose should reach Responded.text"
        );
    }

    #[test]
    fn stream_filter_strips_complete_block_in_one_chunk() {
        let mut f = ToolCallStreamFilter::default();
        let out = f.feed("before <tool_call>edit_file({})</tool_call> after");
        let tail = f.flush();
        let combined = format!("{}{}", out, tail);
        assert!(combined.contains("before "));
        assert!(combined.contains(" after"));
        assert!(!combined.contains("<tool_call>"));
        assert!(!combined.contains("</tool_call>"));
        assert!(!combined.contains("edit_file"));
    }

    #[test]
    fn stream_filter_strips_block_split_across_chunks() {
        // Realistic case: provider streams bytes that split the open tag
        // arbitrarily. The filter must hold back partial-tag bytes, not emit
        // them, and resume cleanly when the close arrives.
        let mut f = ToolCallStreamFilter::default();
        let mut visible = String::new();
        for chunk in [
            "before <tool_",
            "call><tool_name>edit_file</tool_name>",
            "<arg_key>k</arg_key><arg_value>v</arg_value>",
            "</tool_call> after",
        ] {
            visible.push_str(&f.feed(chunk));
        }
        visible.push_str(&f.flush());
        assert_eq!(visible, "before  after");
    }

    #[test]
    fn stream_filter_drops_unclosed_block() {
        // If the stream ends mid-`<tool_call>` (truncation, error), discard
        // the holdback rather than leaking the open fragment to the user.
        let mut f = ToolCallStreamFilter::default();
        let out = f.feed("text <tool_call>edit_file({});");
        let tail = f.flush();
        let combined = format!("{}{}", out, tail);
        assert_eq!(combined, "text ");
    }

    #[test]
    fn stream_filter_handles_partial_open_at_chunk_end() {
        // The filter must not emit `<` or `<t` etc. as visible text just
        // because the chunk happened to end mid-tag.
        let mut f = ToolCallStreamFilter::default();
        let v1 = f.feed("hello <");
        // Could be holdback; not guaranteed any specific output yet.
        let v2 = f.feed("tool_call>x</tool_call>!");
        let tail = f.flush();
        let combined = format!("{}{}{}", v1, v2, tail);
        assert_eq!(combined, "hello !");
    }

    #[test]
    fn stream_filter_passes_through_lt_that_isnt_tool_call() {
        // A bare `<` followed by non-tool_call content should eventually flush.
        let mut f = ToolCallStreamFilter::default();
        let mut visible = String::new();
        visible.push_str(&f.feed("a < b "));
        visible.push_str(&f.feed("and c <"));
        visible.push_str(&f.feed("d>e"));
        visible.push_str(&f.flush());
        assert_eq!(visible, "a < b and c <d>e");
    }

    fn tc(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn repair_fills_missing_old_string_from_xml() {
        // Reproduces the user-reported bug: the function-calling JSON channel
        // delivered `{file_path, new_string, replace_all}` (passes
        // validate_args because new_string is present) but missing
        // `old_string` — execute() then fails with "old_string is required".
        // The full args were carried as XML in the text stream. After repair,
        // the call has all four keys.
        let mut calls = vec![tc(
            "c1",
            "edit_file",
            r#"{"file_path":"x.rs","new_string":"new","replace_all":false}"#,
        )];
        let xml_pool = vec![tc(
            "rescued_0",
            "edit_file",
            r#"{"file_path":"x.rs","old_string":"old","new_string":"new","replace_all":false}"#,
        )];
        repair_tool_call_args(&mut calls, &xml_pool);
        let merged: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(merged["file_path"], "x.rs");
        assert_eq!(merged["old_string"], "old");
        assert_eq!(merged["new_string"], "new");
        assert_eq!(merged["replace_all"], false);
    }

    #[test]
    fn repair_does_not_overwrite_keys_present_in_json() {
        // Conflict policy: function-calling JSON is the source of truth; XML
        // only fills gaps. If the JSON channel and XML disagree on a key
        // (e.g. different new_string), keep JSON.
        let mut calls = vec![tc(
            "c1",
            "edit_file",
            r#"{"file_path":"x.rs","new_string":"json_wins"}"#,
        )];
        let xml_pool = vec![tc(
            "rescued_0",
            "edit_file",
            r#"{"file_path":"x.rs","new_string":"xml_loses","old_string":"old"}"#,
        )];
        repair_tool_call_args(&mut calls, &xml_pool);
        let merged: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(merged["new_string"], "json_wins");
        assert_eq!(merged["old_string"], "old");
    }

    #[test]
    fn repair_skips_when_names_dont_match() {
        // Repair must never cross tool boundaries — patching read_file with
        // edit_file's args would dispatch a malformed call.
        let original = r#"{"file_path":"x.rs"}"#;
        let mut calls = vec![tc("c1", "read_file", original)];
        let xml_pool = vec![tc(
            "rescued_0",
            "edit_file",
            r#"{"file_path":"x.rs","old_string":"a","new_string":"b"}"#,
        )];
        repair_tool_call_args(&mut calls, &xml_pool);
        assert_eq!(calls[0].arguments, original);
    }

    #[test]
    fn repair_takes_xml_wholesale_when_json_unparseable() {
        // Truncated/garbled args from the JSON channel would otherwise
        // fail to parse later anyway. Replace with the XML object.
        let mut calls = vec![tc("c1", "edit_file", r#"{"file_path": "trunc"#)];
        let xml_pool = vec![tc(
            "rescued_0",
            "edit_file",
            r#"{"file_path":"x.rs","old_string":"a","new_string":"b"}"#,
        )];
        repair_tool_call_args(&mut calls, &xml_pool);
        let merged: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(merged["file_path"], "x.rs");
        assert_eq!(merged["old_string"], "a");
    }

    #[test]
    fn repair_matches_multiple_same_name_calls_in_order() {
        // Two edit_file calls in the same turn, two XML blocks — match by
        // order so the second JSON call gets the second XML's args, not the
        // first one's reused.
        let mut calls = vec![
            tc(
                "c1",
                "edit_file",
                r#"{"file_path":"a.rs","new_string":"a_new"}"#,
            ),
            tc(
                "c2",
                "edit_file",
                r#"{"file_path":"b.rs","new_string":"b_new"}"#,
            ),
        ];
        let xml_pool = vec![
            tc(
                "rescued_0",
                "edit_file",
                r#"{"file_path":"a.rs","old_string":"a_old","new_string":"a_new"}"#,
            ),
            tc(
                "rescued_1",
                "edit_file",
                r#"{"file_path":"b.rs","old_string":"b_old","new_string":"b_new"}"#,
            ),
        ];
        repair_tool_call_args(&mut calls, &xml_pool);
        let m1: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        let m2: serde_json::Value = serde_json::from_str(&calls[1].arguments).unwrap();
        assert_eq!(m1["old_string"], "a_old");
        assert_eq!(m2["old_string"], "b_old");
    }

    #[test]
    fn repair_no_op_when_json_already_complete() {
        // If the JSON channel got everything right, repair is silent —
        // serialization-level identity isn't guaranteed (key order may
        // change), but semantic equality holds.
        let mut calls = vec![tc(
            "c1",
            "edit_file",
            r#"{"file_path":"x.rs","old_string":"a","new_string":"b","replace_all":false}"#,
        )];
        let xml_pool = vec![tc(
            "rescued_0",
            "edit_file",
            r#"{"file_path":"x.rs","old_string":"a"}"#,
        )];
        let before: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        repair_tool_call_args(&mut calls, &xml_pool);
        let after: serde_json::Value = serde_json::from_str(&calls[0].arguments).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn repair_skips_unparseable_xml() {
        // If the XML pool has a bogus entry (e.g. arguments that aren't a
        // JSON object), repair must skip it without crashing or polluting
        // the JSON call.
        let original = r#"{"file_path":"x.rs"}"#;
        let mut calls = vec![tc("c1", "edit_file", original)];
        let xml_pool = vec![tc("rescued_0", "edit_file", "not even json")];
        repair_tool_call_args(&mut calls, &xml_pool);
        assert_eq!(calls[0].arguments, original);
    }

    #[test]
    fn stream_filter_handles_utf8_at_holdback_boundary() {
        // UTF-8 multi-byte chars must not get split across the holdback
        // boundary — the trail snap rounds up to a char boundary.
        let mut f = ToolCallStreamFilter::default();
        let mut visible = String::new();
        visible.push_str(&f.feed("中文 hello "));
        visible.push_str(&f.feed("世界"));
        visible.push_str(&f.flush());
        assert_eq!(visible, "中文 hello 世界");
    }
}

/// Build a structured `error_data` JSON for LLM errors, following the
/// telemetry design doc (section 3.5 — `llm_chat` event).
///
/// Extracts `status_code` from the raw error string (patterns like "401",
/// "403", "429", "500", "502", "503") and scrubs the message via
/// `scrub::scrub_path` + `scrub::truncate_head(_, 200)`.
pub(crate) fn build_llm_error_data(
    kind: LlmErrorKind,
    reason: &str,
    duration_ms: u32,
    provider: Option<&str>,
    provider_host: Option<&str>,
    model: Option<&str>,
    context_window: u32,
    system_tokens: u32,
    tool_def_tokens: u32,
    tool_result_tokens: u32,
    message_tokens: u32,
    messages_count: u32,
) -> Option<String> {
    use atomcode_telemetry::scrub;

    // ── Extract status code from the raw error string ──────────────
    let status_code: Option<u16> = extract_status_code(reason);

    // ── Build a concise, scrubbed error message ───────────────────
    // Strip the raw JSON body that some providers append after a colon.
    let home = crate::tool::real_home_dir();
    let cwd = std::env::current_dir().ok();
    let message_raw = scrub::scrub_path(reason, home.as_deref(), cwd.as_deref());
    let message = scrub::truncate_head(&message_raw, 200);

    let base = || -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("duration_ms".into(), serde_json::json!(duration_ms));
        if let Some(p) = provider {
            m.insert("provider".into(), serde_json::json!(p));
        }
        if let Some(h) = provider_host {
            m.insert("provider_host".into(), serde_json::json!(h));
        }
        if let Some(mdl) = model {
            m.insert("model".into(), serde_json::json!(mdl));
        }
        serde_json::Value::Object(m)
    };

    let map = match kind {
        LlmErrorKind::AuthError => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            if let Some(sc) = status_code {
                obj.insert("status_code".into(), serde_json::json!(sc));
            }
            obj.insert("message".into(), serde_json::json!(message));
            m
        }
        LlmErrorKind::RateLimited => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            if let Some(sc) = status_code {
                obj.insert("status_code".into(), serde_json::json!(sc));
            }
            obj.insert("message".into(), serde_json::json!(message));
            // retry_after_secs: could be parsed from Retry-After header,
            // but we don't have that info here. Leave as null.
            obj.insert("retry_after_secs".into(), serde_json::Value::Null);
            m
        }
        LlmErrorKind::ServerError => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            if let Some(sc) = status_code {
                obj.insert("status_code".into(), serde_json::json!(sc));
            }
            obj.insert("message".into(), serde_json::json!(message));
            m
        }
        LlmErrorKind::NetworkError => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            obj.insert("message".into(), serde_json::json!(message));
            obj.insert("attempt_duration_ms".into(), serde_json::json!(duration_ms));
            obj.insert("is_retry".into(), serde_json::json!(false));
            m
        }
        LlmErrorKind::StreamTimeout => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            obj.insert("timeout_secs".into(), serde_json::json!(duration_ms / 1000));
            // Phase heuristic: if no tokens were received → "first_token",
            // otherwise "subsequent". We don't have per-event token counts
            // at this layer, so default to "first_token".
            obj.insert("phase".into(), serde_json::json!("first_token"));
            obj.insert("tokens_received".into(), serde_json::json!(0));
            m
        }
        LlmErrorKind::StreamInterrupted => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            obj.insert("message".into(), serde_json::json!(message));
            obj.insert("bytes_received".into(), serde_json::Value::Null);
            obj.insert("tokens_received".into(), serde_json::Value::Null);
            obj.insert("finish_reason".into(), serde_json::Value::Null);
            m
        }
        LlmErrorKind::ContextOverflow => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            let sent_tokens = system_tokens
                .saturating_add(tool_def_tokens)
                .saturating_add(tool_result_tokens)
                .saturating_add(message_tokens);
            obj.insert("context_window".into(), serde_json::json!(context_window));
            obj.insert("sent_tokens".into(), serde_json::json!(sent_tokens));
            obj.insert("system_tokens".into(), serde_json::json!(system_tokens));
            obj.insert("tool_def_tokens".into(), serde_json::json!(tool_def_tokens));
            obj.insert(
                "tool_result_tokens".into(),
                serde_json::json!(tool_result_tokens),
            );
            obj.insert("message_tokens".into(), serde_json::json!(message_tokens));
            obj.insert("messages_count".into(), serde_json::json!(messages_count));
            m
        }
        LlmErrorKind::Other => {
            let mut m = base();
            let obj = m.as_object_mut().unwrap();
            obj.insert("message".into(), serde_json::json!(message));
            m
        }
    };

    Some(map.to_string())
}

/// Extract an HTTP status code from a raw error string.
/// Looks for patterns like "401", "403", "429", "500", "502", "503"
/// that appear as standalone numbers (not part of a larger number).
fn extract_status_code(reason: &str) -> Option<u16> {
    // Common HTTP error status codes to look for
    let codes = [401u16, 403, 429, 500, 502, 503];
    let lower = reason.to_lowercase();
    for code in codes {
        // Check if the code appears as a standalone number
        // Match patterns like "401", "(401)", "error 401", "HTTP 401"
        let code_str = code.to_string();
        if lower.contains(&code_str) {
            return Some(code);
        }
    }
    None
}

/// Classify an LLM error reason string into a telemetry `LlmErrorKind`.
pub(crate) fn classify_llm_error(reason: &str) -> LlmErrorKind {
    let r = reason.to_lowercase();
    if r.contains("401") || r.contains("403") || r.contains("unauthorized") || r.contains("auth") {
        LlmErrorKind::AuthError
    } else if r.contains("429") || r.contains("rate") || r.contains("throttl") {
        LlmErrorKind::RateLimited
    } else if r.contains("500") || r.contains("502") || r.contains("503") {
        LlmErrorKind::ServerError
    } else if r.contains("stream timeout") || r.contains("no event for") {
        LlmErrorKind::StreamTimeout
    } else if r.contains("decode") || r.contains("mid-flight") || r.contains("terminated") {
        LlmErrorKind::StreamInterrupted
    } else if r.contains("context") || r.contains("max_tokens") || r.contains("token limit") {
        LlmErrorKind::ContextOverflow
    } else if r.contains("connect")
        || r.contains("dns")
        || r.contains("network")
        || r.contains("timeout")
    {
        LlmErrorKind::NetworkError
    } else {
        LlmErrorKind::Other
    }
}

/// Build a concise summary of tool call arguments for telemetry.
/// Extracts top-level JSON keys and truncates values to avoid leaking sensitive data.
pub(crate) fn build_args_summary(tool_name: &str, args: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        if let Some(obj) = v.as_object() {
            let pairs: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    let val_str = match v {
                        serde_json::Value::String(s) => {
                            atomcode_telemetry::scrub::truncate_head(s, 50)
                        }
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        serde_json::Value::Null => "null".to_string(),
                        _ => format!(
                            "<{}>",
                            match v {
                                serde_json::Value::Array(_) => "array",
                                serde_json::Value::Object(_) => "object",
                                _ => "value",
                            }
                        ),
                    };
                    format!("{}={}", k, val_str)
                })
                .collect();
            return format!("{}({})", tool_name, pairs.join(", "));
        }
    }
    // Fallback: truncate raw args
    format!(
        "{}({})",
        tool_name,
        atomcode_telemetry::scrub::truncate_head(args, 100)
    )
}
