//! Default render & compression-plan policy for atomcode ctx.
//!
//! [`build_messages`], [`needs_compression`], and
//! [`build_compression_content`] implement the out-of-the-box context
//! behavior. `DefaultCtx` is a thin wrapper over them; `OllamaCtx`
//! reuses `build_messages` / `build_compression_content` and overrides
//! only the compression threshold (early trigger).
//!
//! Implementations wanting different behavior (different thresholds,
//! different compression content format, different cold-zone layout)
//! write their own `impl CtxBuilder` without touching this module.
//!
//! All functions here are free functions taking `&Conversation`,
//! keeping `Conversation` as a pure data container — no render logic
//! leaks back into the data layer.

use crate::conversation::{Conversation, ContextStats, KEEP_MESSAGES};
use crate::conversation::message::{self, Message, MessageContent, Role};

/// Render the per-turn dynamic reminder string from agent-owned state.
///
/// Default policy (used by [`crate::ctx::DefaultCtx`] / [`crate::ctx::OllamaCtx`]):
///
/// - If `prev_edited_files` is non-empty, emit a one-line hint pointing
///   the model at last turn's touched files (avoids redundant search
///   when the user follows up on the same area).
/// - If `current_task` is non-empty, emit a `=== CURRENT TASK ===`
///   block at the very end (recency: the last ~200 prompt tokens are
///   what the model attends to most when starting a turn). Truncated
///   to ~300 chars to bound the injection size.
///
/// Returns `String::new()` when both inputs are empty — callers should
/// treat empty as "no reminder this turn" and skip injection entirely.
///
/// Per-ctx impls override [`crate::ctx::CtxBuilder::render_turn_reminder`]
/// when they want different placement (e.g. a future ClaudeCtx may want
/// the reminder as its own System message to keep cache prefix stable;
/// a small-window ctx may want to drop it entirely to save tokens).
pub fn render_turn_reminder(prev_edited_files: &[String], current_task: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    if !prev_edited_files.is_empty() {
        let files = prev_edited_files.join(", ");
        parts.push(format!(
            "[Previous turn: you edited {}. If the user reports the same issue, start from these files.]",
            files
        ));
    }

    if !current_task.is_empty() {
        let task_short = if current_task.chars().count() > 300 {
            format!("{}...", current_task.chars().take(297).collect::<String>())
        } else {
            current_task.to_string()
        };
        parts.push(format!(
            "=== CURRENT TASK ===\n{}\nAct on this task directly. Do NOT search for files you already know about.",
            task_short
        ));
    }

    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n\n")
    }
}

/// Append model-specific behavioral directives to a system prompt.
///
/// Previously scattered as `if model_id.contains(...)` branches inside
/// `agent::prompt::build_system_prompt`. Moved here so per-model prompt
/// customization lives in the ctx layer alongside other per-model logic
/// (compression threshold, tool-output cap, etc).
///
/// `model_id` MUST already be lowercased by the caller (matching the
/// original `provider.model.to_lowercase()` check).
///
/// Currently handles two groups:
/// - CN language lock: minimax / qwen / deepseek / kimi models default
///   to English reasoning even when the user speaks Chinese; one gentle
///   line nudges user-visible output back to zh-CN.
/// - MiniMax thinking discipline: MiniMax M2 has no reasoning_effort
///   knob and defaults to extremely verbose `<think>` blocks; a
///   system-reminder near the tail caps it to ≤3 sentences via recency
///   bias.
///
/// Impls that don't want these (e.g. a hypothetical ClaudeCtx) simply
/// don't call this function — the hooks live in each `build_messages`
/// impl, not in `ctx::render::build_messages`.
pub(crate) fn apply_model_directives(system_prompt: &str, model_id: &str) -> String {
    let mut out = String::with_capacity(system_prompt.len() + 512);
    out.push_str(system_prompt);

    let needs_cn_lock = model_id.contains("minimax")
        || model_id.contains("qwen")
        || model_id.contains("deepseek")
        || model_id.contains("kimi");
    if needs_cn_lock {
        out.push_str("\n用户可见的输出请用中文。工具调用和代码保持原样。\n");
    }

    // MiniMax M2 的 thinking 默认极其啰嗦，会大量消耗 output tokens 并拖慢响应。
    // 模型本身没有 reasoning_effort 档位开关，只能用 prompt 约束。放在接近尾部
    // 借助 recency 保证每轮都生效，等效于一个轻量 system-reminder。
    if model_id.contains("minimax") {
        out.push_str(
            "\n<system-reminder>\n\
             THINKING 简洁纪律：内部思考（<think> 块）必须极简，\
             只写必要的决策线索，不要复述工具结果、不要分点展开、不要自问自答。\
             目标 ≤ 3 句话。冗长 thinking 视为严重问题。\n\
             </system-reminder>\n",
        );
    }

    out
}

/// Context management with cold zone compression.
///
/// Structure: [System] [Cold Zone (max 3 summaries)] [Last 5 turns full]
///
/// The cold zone is populated by `Conversation::apply_compression` when
/// total tokens exceed ~70% of budget. If still over 80% after cold zone
/// injection, this function drops oldest turns inline.
///
/// `turn_reminder` — if non-empty, prepended to the last User message.
/// Keeps the system prompt prefix stable across turns (好 cache),
/// while still delivering per-turn dynamic context (git diff, current
/// task, etc). Empty string = no injection.
pub fn build_messages(
    conv: &Conversation,
    system_prompt: &str,
    token_budget: usize,
    turn_reminder: &str,
) -> (Vec<Message>, ContextStats) {
    if conv.messages.is_empty() {
        return (vec![Message::new(Role::System, system_prompt)], ContextStats::default());
    }

    let system_msg = Message::new(Role::System, system_prompt);
    let system_tokens = system_msg.estimate_tokens();

    let turns = &conv.turn_tracker.turns;

    if turns.is_empty() {
        let remaining = token_budget.saturating_sub(system_tokens);
        return (build_messages_fallback(conv, system_msg, remaining), ContextStats::default());
    }

    let mut result = Vec::with_capacity(conv.messages.len() + 3);
    result.push(system_msg);

    // Inject cold zone summaries (if any)
    if !conv.cold_summaries.is_empty() {
        let cold_text = format!(
            "[Earlier conversation history ({} compression{})]\n{}",
            conv.cold_summaries.len(),
            if conv.cold_summaries.len() > 1 { "s" } else { "" },
            conv.cold_summaries.join("\n---\n")
        );
        result.push(Message::new(Role::System, cold_text));
    }

    // Add all current messages
    result.extend(conv.messages.iter().cloned());

    // NOTE: read_file result condensation was here (83fc7ff) but reverted.
    // 问题: 长距离重读是合理需求（旧内容被压缩后模型需要重新看），
    // 短距离重读在 keep_recent 保护内又压缩不到。两头不讨好。
    // 正确方案需要更深入设计，不在这里做。

    // Safety: if over 80% (or 60K absolute cap), drop oldest turns.
    // BUT: skip if cold_summaries exist — that means LLM compression just ran
    // and we're looking at the "keep_full=5" survivor set. Dropping those too
    // would wipe ALL context (the bug that caused sent=0 in audit sessions).
    let budget_80pct = (token_budget * 80 / 100).min(60000);
    let total_tokens: usize = result.iter().map(|m| m.estimate_tokens()).sum();
    let mut dropped_tokens = 0usize;

    if total_tokens > budget_80pct && conv.cold_summaries.is_empty() {
        let tokens_to_drop = total_tokens - budget_80pct;

        // ── HARD FLOOR: the last turn is sacred and NEVER dropped ──
        // Without this floor, a single oversized tool_result could make `tokens_to_drop`
        // exceed the sum of all earlier turns, and the `survived_start` calculation below
        // would settle on `conv.messages.len()` → NO messages survive → sent=0 → agent
        // goes blind and repeats searches forever (2026-04-12 21:25 session pathology).
        let last_turn_idx = turns.len().saturating_sub(1);
        let last_turn_start = turns.get(last_turn_idx)
            .map(|t| t.start_idx)
            .unwrap_or(0)
            .min(conv.messages.len());

        // First pass: identify which turns to drop and extract their reasoning.
        // Loop bound `turns.len()-1` ensures we never touch the last turn.
        let mut drop_summaries: Vec<String> = Vec::new();
        let mut drop_count = 0usize;

        for ti in 0..turns.len().saturating_sub(1) {
            if dropped_tokens >= tokens_to_drop { break; }
            let turn = &turns[ti];
            let end = turn.end_idx().min(conv.messages.len());
            if turn.start_idx >= conv.messages.len() { continue; }

            // Extract model reasoning and tool calls before dropping
            let turn_msgs = &conv.messages[turn.start_idx..end];
            let mut parts: Vec<String> = Vec::new();
            for msg in turn_msgs {
                match &msg.content {
                    MessageContent::Text(t) if msg.role == Role::Assistant => {
                        let short: String = t.chars().take(150).collect();
                        if !short.trim().is_empty() {
                            parts.push(short);
                        }
                    }
                    MessageContent::AssistantWithToolCalls { text, tool_calls, .. } => {
                        if let Some(t) = text {
                            let short: String = t.chars().take(150).collect();
                            if !short.trim().is_empty() {
                                parts.push(short);
                            }
                        }
                        let tools: Vec<&str> = tool_calls.iter()
                            .map(|tc| tc.name.as_str()).collect();
                        if !tools.is_empty() {
                            parts.push(format!("tools: {}", tools.join(", ")));
                        }
                    }
                    _ => {}
                }
            }
            if !parts.is_empty() {
                drop_summaries.push(parts.join(" | "));
            }

            dropped_tokens += turn_msgs.iter()
                .map(|m| m.estimate_tokens()).sum::<usize>();
            drop_count += 1;
        }

        // Rebuild: system + cold zone + drop digest + surviving messages
        let cold_msgs = if conv.cold_summaries.is_empty() { 1 } else { 2 };
        result.truncate(cold_msgs);

        // Inject mechanical digest of dropped turns so model retains reasoning chain
        if !drop_summaries.is_empty() {
            let digest = format!(
                "[Context overflow: {} earlier turns compressed]\n{}",
                drop_count,
                drop_summaries.iter().enumerate()
                    .map(|(i, s)| format!("{}. {}", i + 1, s))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            result.push(Message::new(Role::System, digest));
        }

        // Find first surviving message, clamped to last_turn_start so the last turn always survives.
        let mut survived_start = 0;
        let mut skipped = 0usize;
        for ti in 0..turns.len() {
            let turn = &turns[ti];
            let end = turn.end_idx().min(conv.messages.len());
            if turn.start_idx >= conv.messages.len() { continue; }
            let t: usize = conv.messages[turn.start_idx..end]
                .iter().map(|m| m.estimate_tokens()).sum();
            skipped += t;
            if skipped >= dropped_tokens {
                survived_start = if ti + 1 < turns.len() {
                    turns[ti + 1].start_idx
                } else {
                    // Old code set this to conv.messages.len() → no survivors.
                    // Clamp to last_turn_start to preserve at least the last turn.
                    last_turn_start
                };
                break;
            }
        }
        // Final clamp: survived_start must not skip past the last turn.
        survived_start = survived_start.min(last_turn_start);
        result.extend(conv.messages[survived_start..].iter().cloned());
    }

    // Microcompact: condense old turn ToolResults to one-liners.
    // Recent 5 turns keep full fidelity. Older turns' large tool results
    // (read_file full content, bash output) are replaced with compact summaries.
    // This reduces context growth without LLM calls.
    // View replacement runs AFTER microcompact — edited files stay fresh.
    microcompact(&mut result, conv.messages.len());

    replace_stale_reads(&mut result);
    clean_message_pipeline(&mut result);

    // ── ABSOLUTE FLOOR (runs AFTER all cleanup, right before sent_tokens calc) ──
    // If compaction + cleanup somehow left us with only system messages, graft back
    // the last user message so the LLM has *something* to respond to. This is the
    // strictest possible invariant: whenever conv.messages is non-empty, the result
    // must contain at least one non-system message.
    let non_system_count = result.iter().filter(|m| !matches!(m.role, Role::System)).count();
    if non_system_count == 0 {
        if let Some(last_user) = conv.messages.iter().rev()
            .find(|m| matches!(m.role, Role::User) && matches!(m.content, MessageContent::Text(..)))
        {
            result.push(Message::new(
                Role::System,
                "[Emergency: prior conversation was dropped during compaction. Only the latest user message is preserved.]"
            ));
            result.push(last_user.clone());
        }
    }

    // Turn reminder: prepend to last User message. Runs AFTER all
    // compaction/cleanup so the reminder always rides the most recent
    // user turn. Keeps system_prompt itself stable (cacheable).
    if !turn_reminder.is_empty() {
        for msg in result.iter_mut().rev() {
            if matches!(msg.role, Role::User) {
                if let MessageContent::Text(ref mut text) = msg.content {
                    *text = format!("{}\n{}", turn_reminder, text);
                    break;
                }
            }
        }
    }

    let sent_tokens: usize = result.iter().map(|m| m.estimate_tokens()).sum::<usize>()
        .saturating_sub(system_tokens);
    let msg_count = result.len();
    (result, ContextStats {
        system_tokens,
        sent_tokens,
        dropped_tokens,
        total_messages: msg_count,
    })
}

/// Check if context needs compression.
///
/// Threshold: `min(50% of budget, 50K tokens)`. Stable across many real
/// sessions — do NOT lower without validating on long write-heavy
/// sessions (agentarena) — 55% caused total context wipeout historically.
pub fn needs_compression(
    conv: &Conversation,
    system_prompt_tokens: usize,
    token_budget: usize,
) -> bool {
    // Guard: need enough messages to make compression worthwhile.
    // Uses message count instead of turn count because turn_tracker counts
    // USER MESSAGES (1 user msg = 1 turn), but a single user message can
    // produce 15+ LLM calls with 35+ messages. The old `turns.len() < 6`
    // guard caused compression to NEVER trigger in agent-loop scenarios.
    if conv.messages.len() < 12 { return false; }
    let total: usize = system_prompt_tokens + conv.messages.iter()
        .map(|m| m.estimate_tokens()).sum::<usize>();
    let threshold = (token_budget * 50 / 100).min(50000);
    total > threshold
}

/// Build content for LLM compression.
///
/// Strategy: keep the last `KEEP_MESSAGES` messages at full fidelity,
/// compress everything before that into one-line-per-round summaries.
/// Returns `(compressed_text, number_of_messages_to_remove)`.
///
/// This operates at MESSAGE level, not turn level, because `turn_tracker`
/// counts user messages (1 user msg = 1 turn) but a single user message
/// can produce 15+ LLM calls with 35+ messages.
pub fn build_compression_content(conv: &Conversation) -> (String, usize) {
    if conv.messages.len() <= KEEP_MESSAGES {
        return (String::new(), 0);
    }

    let compress_end_idx = conv.messages.len() - KEEP_MESSAGES;

    // Group messages into logical rounds (assistant + tool_calls + tool_results)
    // and compress each round into a one-liner.
    let mut content = String::new();
    let mut round = 0usize;
    let compress_msgs = &conv.messages[..compress_end_idx];
    let mut i = 0;
    while i < compress_msgs.len() {
        // Collect messages for this round
        let round_start = i;
        // A round starts at a User or Assistant message and includes
        // all subsequent tool results until the next User/Assistant.
        i += 1;
        while i < compress_msgs.len() {
            match compress_msgs[i].role {
                message::Role::User | message::Role::Assistant => break,
                _ => i += 1,
            }
        }
        round += 1;
        let round_msgs = &compress_msgs[round_start..i];
        content.push_str(&compress_turn(round, round_msgs));
        content.push('\n');
    }

    // Return message count (not turn count) for apply_compression
    (content, compress_end_idx)
}

// ─── private helpers ────────────────────────────────────────────────

/// Compress a turn into a one-line mechanical summary.
/// No LLM call — deterministic, fast, never fails.
/// Format: "Turn N: user asked X → read file.js, edited file.js (-3 +5 lines)"
// ── INVARIANT (2026-04-16): compress_turn MUST preserve assistant thinking ──
// The assistant's text (thinking/reasoning) in AssistantWithToolCalls is the
// diagnostic conclusion for that turn ("代码逻辑看起来正确", "问题找到了！ID不匹配").
// Without it, the compressed summary says only "read main.ts, grep closeSettings"
// — the model doesn't know it already confirmed the logic was correct, so it
// searches the same files again. 39-turn loop sessions traced to this omission.
fn compress_turn(turn_num: usize, turn_msgs: &[Message]) -> String {
    let mut user_text = String::new();
    let mut assistant_text = String::new();
    let mut tools: Vec<String> = Vec::new();

    for msg in turn_msgs {
        match (&msg.role, &msg.content) {
            (Role::User, MessageContent::Text(s)) => {
                if !s.starts_with('[') { // skip system-injected messages
                    user_text = if s.chars().count() > 60 {
                        format!("{}...", s.chars().take(57).collect::<String>())
                    } else {
                        s.clone()
                    };
                }
            }
            (_, MessageContent::AssistantWithToolCalls { text, tool_calls }) => {
                // Preserve assistant's diagnostic conclusion (first 80 chars).
                if let Some(t) = text {
                    let trimmed = t.trim();
                    if !trimmed.is_empty() && assistant_text.is_empty() {
                        assistant_text = if trimmed.chars().count() > 80 {
                            format!("{}...", trimmed.chars().take(77).collect::<String>())
                        } else {
                            trimmed.to_string()
                        };
                    }
                }
                for tc in tool_calls {
                    let short = if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                        let fp = args.get("file_path").and_then(|v| v.as_str())
                            .map(|p| std::path::Path::new(p).file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.to_string()));
                        match (tc.name.as_str(), fp) {
                            ("read_file", Some(f)) => format!("read {}", f),
                            ("edit_file", Some(f)) => format!("edit {}", f),
                            ("write_file", Some(f)) => format!("write {}", f),
                            ("grep", _) => {
                                let pat = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
                                format!("grep({})", pat)
                            }
                            ("bash", _) => {
                                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("?");
                                let short_cmd: String = cmd.chars().take(30).collect();
                                format!("bash({})", short_cmd)
                            }
                            (name, _) => name.to_string(),
                        }
                    } else {
                        tc.name.clone()
                    };
                    if !tools.contains(&short) {
                        tools.push(short);
                    }
                }
            }
            (Role::Assistant, MessageContent::Text(s)) => {
                if assistant_text.is_empty() {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        assistant_text = if trimmed.chars().count() > 80 {
                            format!("{}...", trimmed.chars().take(77).collect::<String>())
                        } else {
                            trimmed.to_string()
                        };
                    }
                }
            }
            (_, MessageContent::ToolResult(r)) if !r.success => {
                tools.push("FAILED".to_string());
            }
            _ => {}
        }
    }

    let tools_str = if tools.is_empty() { "no tools".to_string() } else { tools.join(", ") };

    let prefix = if !user_text.is_empty() {
        format!("\"{}\" ", user_text)
    } else {
        String::new()
    };
    let conclusion = if !assistant_text.is_empty() {
        format!("[{}] ", assistant_text)
    } else {
        String::new()
    };
    format!("- Turn {}: {}{}→ {}", turn_num, prefix, conclusion, tools_str)
}

/// Fallback windowing when no turns are tracked.
/// Keeps as many recent messages as fit within 60% of remaining budget.
fn build_messages_fallback(
    conv: &Conversation,
    system_msg: Message,
    remaining_budget: usize,
) -> Vec<Message> {
    let budget = remaining_budget * 60 / 100;
    let mut used = 0usize;
    let mut start = conv.messages.len();

    for i in (0..conv.messages.len()).rev() {
        let msg_tokens = conv.messages[i].estimate_tokens();
        if used + msg_tokens > budget {
            break;
        }
        used += msg_tokens;
        start = i;
    }
    start = snap_to_valid_boundary(&conv.messages, start);

    let mut result = Vec::with_capacity(conv.messages.len() - start + 1);
    result.push(system_msg);
    result.extend(conv.messages[start..].iter().cloned());
    sanitize_messages(&mut result);
    result
}

/// Snap an index to a valid message boundary for the API.
fn snap_to_valid_boundary(messages: &[Message], idx: usize) -> usize {
    let mut start = idx.min(messages.len());

    // Skip orphan ToolResult/ToolResultRef messages
    while start < messages.len() {
        match &messages[start].content {
            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => start += 1,
            _ => break,
        }
    }

    // Prefer starting at a User message
    let original = start;
    while start < messages.len() {
        if matches!(messages[start].role, Role::User | Role::System) {
            break;
        }
        start += 1;
        if start > original + 5 {
            return original;
        }
    }
    start
}

// ─── Message-list manipulation helpers used during render ───────────
// These operate on `&mut Vec<Message>` and are called by
// `build_messages` to apply rolling condensation / freshness
// replacement / sanity cleanup.

/// Microcompact: condense old ToolResult messages to one-line summaries.
/// Zero LLM calls — purely mechanical compression.
///
/// `read_file` results are NEVER condensed by microcompact. They stay in
/// context for the entire task so the model can cross-reference files
/// freely. Cleanup happens at two higher levels:
/// 1. Task boundary compression (new user message → old task compressed)
/// 2. 50% LLM compression threshold (context > 32K → oldest turns compressed)
///
/// Other tool results (bash, grep, edit, etc.) are condensed after
/// 20 messages to keep context growth in check.
fn microcompact(msgs: &mut Vec<Message>, total_msg_count: usize) {
    const OTHER_KEEP: usize = 20;

    let total_chars: usize = msgs.iter().map(|m| {
        match &m.content {
            MessageContent::ToolResult(r) => r.output.len(),
            MessageContent::Text(t) => t.len(),
            _ => 100,
        }
    }).sum();
    if total_chars < 100_000 { return; }
    if total_msg_count <= OTHER_KEEP { return; }

    let other_cutoff = total_msg_count.saturating_sub(OTHER_KEEP);

    let mut call_id_to_tool: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for msg in msgs.iter() {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                call_id_to_tool.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }

    let cold_msgs = msgs.iter()
        .position(|m| !matches!(m.role, Role::System))
        .unwrap_or(0);

    let condense_end = cold_msgs + other_cutoff;

    for i in cold_msgs..condense_end.min(msgs.len()) {
        if let MessageContent::ToolResult(ref r) = msgs[i].content {
            let _tool_name = call_id_to_tool.get(&r.call_id)
                .map(|s| s.as_str())
                .unwrap_or("tool");

            let msg_idx = i.saturating_sub(cold_msgs);
            if msg_idx >= other_cutoff { continue; }

            if r.output.len() <= 500 { continue; }

            let tool_name = call_id_to_tool.get(&r.call_id)
                .map(|s| s.as_str())
                .unwrap_or("tool");

            let summary = match tool_name {
                "read_file" => {
                    let line_count = r.output.lines().count();
                    let first_line = r.output.lines().next().unwrap_or("");
                    let hint: String = first_line.chars().take(60).collect();
                    format!("[Read file ({} lines): {}]", line_count, hint)
                }
                "bash" => {
                    let first_line = r.output.lines().next().unwrap_or("(empty)");
                    let line_count = r.output.lines().count();
                    let short: String = first_line.chars().take(80).collect();
                    if r.success {
                        format!("[bash ({} lines): {}]", line_count, short)
                    } else {
                        format!("[bash FAILED ({} lines): {}]", line_count, short)
                    }
                }
                "grep" => {
                    let match_count = r.output.lines().filter(|l| l.contains(':')).count();
                    format!("[grep: {} matches]", match_count)
                }
                "glob" => {
                    let file_count = r.output.lines().count();
                    format!("[glob: {} files]", file_count)
                }
                _ => {
                    let first_line = r.output.lines().next().unwrap_or("");
                    let short: String = first_line.chars().take(80).collect();
                    format!("[{}: {}]", tool_name, short)
                }
            };

            msgs[i].content = MessageContent::ToolResult(crate::tool::ToolResult {
                call_id: r.call_id.clone(),
                output: summary,
                success: r.success,
            });
        }
    }
}

/// Replace stale read_file results with current disk content.
/// When a file was read then later edited, the old read result is outdated.
/// This replaces it so the model always sees the latest version.
fn replace_stale_reads(msgs: &mut Vec<Message>) {
    struct ReadInfo {
        file_path: String,
        offset: Option<usize>,
        limit: Option<usize>,
    }
    let mut call_id_to_read: std::collections::HashMap<String, ReadInfo> = std::collections::HashMap::new();
    let mut edit_call_to_file: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut edited_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in msgs.iter() {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    let file_path = args.get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if tc.name == "read_file" && !file_path.is_empty() {
                        let offset = args.get("offset").and_then(|v| v.as_u64()).map(|v| v as usize);
                        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
                        call_id_to_read.insert(tc.id.clone(), ReadInfo { file_path: file_path.clone(), offset, limit });
                    }
                    if matches!(tc.name.as_str(), "edit_file" | "write_file" | "create_file") && !file_path.is_empty() {
                        edit_call_to_file.insert(tc.id.clone(), file_path);
                    }
                }
            }
        }
        if let MessageContent::ToolResult(ref r) = msg.content {
            if let Some(file_path) = edit_call_to_file.get(&r.call_id) {
                if !r.output.starts_with("Error") {
                    edited_files.insert(file_path.clone());
                }
            }
        }
    }

    if edited_files.is_empty() {
        return;
    }

    for msg in msgs.iter_mut() {
        if let MessageContent::ToolResult(ref mut r) = msg.content {
            if let Some(info) = call_id_to_read.get(&r.call_id) {
                if !edited_files.contains(&info.file_path) { continue; }
                if let Ok(content) = std::fs::read_to_string(&info.file_path) {
                    let all_lines: Vec<&str> = content.lines().collect();
                    let total = all_lines.len();

                    if info.offset.is_some() || info.limit.is_some() {
                        let start = info.offset.unwrap_or(1).max(1) - 1;
                        let start = start.min(total);
                        let end = info.limit.map(|l| (start + l).min(total)).unwrap_or(total);
                        let display: String = all_lines[start..end].iter().enumerate()
                            .map(|(i, l)| format!("{:>4}| {}", start + i + 1, l))
                            .collect::<Vec<_>>()
                            .join("\n");
                        r.output = display;
                    } else if total <= 300 {
                        r.output = all_lines.iter().enumerate()
                            .map(|(i, l)| format!("{:>4}| {}", i + 1, l))
                            .collect::<Vec<_>>()
                            .join("\n");
                    }
                    // else: large-file full-read, keep existing skeleton as-is.
                }
            }
        }
    }
}

/// Walk forward tracking tool_call/tool_result pairing; remove orphans.
/// Valid sequences: System → (User → Assistant/AssistantWithToolCalls → [ToolResult]* → ...)*
fn sanitize_messages(msgs: &mut Vec<Message>) {
    let mut to_remove: Vec<usize> = Vec::new();
    let mut expecting_tool_results = 0usize;

    for i in 0..msgs.len() {
        match &msgs[i].content {
            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                if expecting_tool_results > 0 {
                    expecting_tool_results -= 1;
                } else {
                    to_remove.push(i);
                }
            }
            MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                expecting_tool_results = tool_calls.len();
            }
            MessageContent::Text(_) => {
                expecting_tool_results = 0;
            }
        }
    }

    if expecting_tool_results > 0 {
        for i in (0..msgs.len()).rev() {
            match &msgs[i].content {
                MessageContent::AssistantWithToolCalls { .. } => {
                    to_remove.push(i);
                    break;
                }
                MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                    to_remove.push(i);
                }
                _ => break,
            }
        }
    }

    to_remove.sort_unstable();
    to_remove.dedup();
    for &idx in to_remove.iter().rev() {
        msgs.remove(idx);
    }
}

/// Clean message pipeline before sending to API.
/// Removes noise that degrades model decision quality:
/// - Empty/whitespace-only assistant messages
/// - Orphaned tool results (no matching tool_use)
/// - Consecutive same-role user messages (merge into one)
fn clean_message_pipeline(msgs: &mut Vec<Message>) {
    // 1. Remove empty assistant messages (e.g., after <think> stripping)
    msgs.retain(|m| {
        if m.role == Role::Assistant {
            match &m.content {
                MessageContent::Text(t) => !t.trim().is_empty(),
                _ => true,
            }
        } else {
            true
        }
    });

    // 2. Collect valid tool_use IDs from assistant messages
    let mut valid_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for msg in msgs.iter() {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                valid_call_ids.insert(tc.id.clone());
            }
        }
    }

    // 3. Remove orphaned tool results (no matching tool_use)
    msgs.retain(|m| {
        if let MessageContent::ToolResult(ref r) = m.content {
            valid_call_ids.contains(&r.call_id)
        } else if let MessageContent::ToolResultRef(ref r) = m.content {
            valid_call_ids.contains(&r.call_id)
        } else {
            true
        }
    });

    // 4. Merge consecutive user messages into one
    let mut i = 1;
    while i < msgs.len() {
        if msgs[i].role == Role::User && msgs[i - 1].role == Role::User {
            if let (MessageContent::Text(prev), MessageContent::Text(curr)) =
                (&msgs[i - 1].content, &msgs[i].content)
            {
                let merged = format!("{}\n{}", prev, curr);
                msgs[i - 1].content = MessageContent::Text(merged);
                msgs.remove(i);
                continue;
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::Conversation;
    use crate::conversation::message::{Message, Role};

    #[test]
    fn render_turn_reminder_empty_when_no_state() {
        assert_eq!(render_turn_reminder(&[], ""), "");
    }

    #[test]
    fn render_turn_reminder_includes_prev_files_only() {
        let files = vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()];
        let out = render_turn_reminder(&files, "");
        assert!(out.contains("src/foo.rs, src/bar.rs"));
        assert!(out.contains("Previous turn"));
        assert!(!out.contains("CURRENT TASK"));
    }

    #[test]
    fn render_turn_reminder_includes_current_task_only() {
        let out = render_turn_reminder(&[], "fix the auth bug");
        assert!(out.contains("CURRENT TASK"));
        assert!(out.contains("fix the auth bug"));
        assert!(!out.contains("Previous turn"));
    }

    #[test]
    fn render_turn_reminder_truncates_long_task_at_300_chars() {
        let task = "x".repeat(500);
        let out = render_turn_reminder(&[], &task);
        // 297 'x' + "..." + framing text — task body should be capped near 300
        assert!(out.contains("xxx..."));
        // ensure truncation happened (not all 500 x's verbatim)
        assert!(!out.contains(&"x".repeat(400)));
    }

    #[test]
    fn render_turn_reminder_task_appears_after_prev_files() {
        // recency: task block must come last (it's the model's first focus)
        let files = vec!["a.rs".to_string()];
        let out = render_turn_reminder(&files, "do thing");
        let prev_idx = out.find("Previous turn").unwrap();
        let task_idx = out.find("CURRENT TASK").unwrap();
        assert!(task_idx > prev_idx);
    }

    #[test]
    fn apply_model_directives_noop_for_generic_model() {
        // gpt / claude / gemini 等模型不触发任何指令 — 原 prompt 原样返回。
        let out = apply_model_directives("SYS", "gpt-4o");
        assert_eq!(out, "SYS");
        let out = apply_model_directives("SYS", "claude-opus-4-7");
        assert_eq!(out, "SYS");
    }

    #[test]
    fn apply_model_directives_cn_lock_for_cjk_tier() {
        for id in ["qwen3-max", "deepseek-v3", "kimi-k2"] {
            let out = apply_model_directives("SYS", id);
            assert!(
                out.contains("用户可见的输出请用中文"),
                "model {id} missing CN lock"
            );
            assert!(!out.contains("THINKING 简洁纪律"), "model {id} got MiniMax directive erroneously");
        }
    }

    #[test]
    fn apply_model_directives_minimax_gets_both_blocks() {
        let out = apply_model_directives("SYS", "minimax-m2");
        assert!(out.contains("用户可见的输出请用中文"));
        assert!(out.contains("THINKING 简洁纪律"));
        // MiniMax 指令必须在 CN lock 之后(recency: 更尾部 = 更高优先级)
        let cn_idx = out.find("用户可见的输出").unwrap();
        let thinking_idx = out.find("THINKING").unwrap();
        assert!(thinking_idx > cn_idx);
    }

    #[test]
    fn apply_model_directives_preserves_system_prompt_prefix() {
        // 追加模式:原 prompt 必须 100% 保留在开头,cache key 不破坏。
        let sys = "You are AtomCode. Working directory: /tmp\n";
        let out = apply_model_directives(sys, "minimax-m2");
        assert!(out.starts_with(sys));
    }

    #[test]
    fn test_budgeted_empty_conversation() {
        let conv = Conversation::new();
        let (msgs, _stats) = build_messages(&conv, "system prompt", 8000, "");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::System));
    }


    #[test]
    fn test_budgeted_includes_recent_messages() {
        let mut conv = Conversation::new();
        conv.add_user_message("hello");
        conv.messages.push(Message::new(Role::Assistant, "hi there"));
        conv.add_user_message("do something");

        let (msgs, _stats) = build_messages(&conv, "sys", 8000, "");
        assert_eq!(msgs.len(), 4); // system + 3 messages
        assert!(matches!(msgs[0].role, Role::System));
    }


    #[test]
    fn test_budgeted_sends_all_when_under_80pct() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 2 turns with small tool results — should all fit
        for turn in 0..2 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("call_{}", turn),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/tmp/file_{}.rs"}}"#, turn),
            };
            conv.add_assistant_tool_calls(None, vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", turn),
                output: "short result".to_string(),
                success: true,
            });
        }
        conv.add_user_message("now what?");

        // Large budget — everything fits
        let (msgs, stats) = build_messages(&conv, "sys", 100000, "");
        // system + 7 messages (2 turns * 3 msgs each + final user)
        assert_eq!(msgs.len(), 8);
        assert!(matches!(msgs[0].role, Role::System));
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
        assert_eq!(stats.dropped_tokens, 0, "Nothing should be dropped");
    }


    #[test]
    fn test_budgeted_drops_oldest_turns_when_over_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 5 turns with large tool results (2000 chars each ≈ 500 tokens)
        // Total ≈ 5 * 4 * 500 = 10000 tokens + overhead, budget 80% of 4000 = 3200
        for turn in 0..5 {
            conv.add_user_message(&format!("task {}", turn));
            for i in 0..4 {
                let idx = turn * 4 + i;
                let call = ToolCall {
                    id: format!("call_{}", idx),
                    name: "read_file".to_string(),
                    arguments: format!(r#"{{"file_path":"/tmp/file_{}.rs"}}"#, idx),
                };
                conv.add_assistant_tool_calls(None, vec![call]);
                conv.add_tool_result(ToolResult {
                    call_id: format!("call_{}", idx),
                    output: "x".repeat(2000),
                    success: true,
                });
            }
        }
        conv.add_user_message("now what?");

        let (msgs, stats) = build_messages(&conv, "sys", 4000, "");
        // Oldest turns should be dropped
        assert!(stats.dropped_tokens > 0, "Some turns should have been dropped");
        // Most recent user message must survive
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
        // System prompt must be first
        assert!(matches!(msgs[0].role, Role::System));
    }


    #[test]
    fn test_budgeted_always_keeps_latest_turn() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create a single turn with very large output
        conv.add_user_message("big task");
        let call = ToolCall {
            id: "c0".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };
        conv.add_assistant_tool_calls(Some("running..."), vec![call]);
        conv.add_tool_result(ToolResult {
            call_id: "c0".to_string(),
            output: "z".repeat(50000),
            success: true,
        });

        // Very small budget — system prompt is always kept
        let (msgs, _stats) = build_messages(&conv, "sys", 1000, "");
        assert!(!msgs.is_empty(), "Must at least have system prompt");
        assert!(matches!(msgs[0].role, Role::System));
    }


    #[test]
    fn test_budgeted_never_returns_system_only_when_messages_exist() {
        // Regression for 2026-04-13 bug: a single oversized tool_result caused
        // `survived_start = self.messages.len()` → no non-system messages in result
        // → sent=0 → agent blind.
        //
        // Invariant: if self.messages is non-empty, to_provider_messages_budgeted
        // must always include at least one non-system message.
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // 5 normal turns
        for i in 0..5 {
            conv.add_user_message(&format!("task {}", i));
            let call = ToolCall {
                id: format!("c{}", i),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", i),
                output: "x".repeat(500),
                success: true,
            });
        }

        // 6th turn with a pathologically oversized output (50K tokens worth of 'z')
        conv.add_user_message("find everything");
        let call = ToolCall {
            id: "c5".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };
        conv.add_assistant_tool_calls(Some("finding..."), vec![call]);
        conv.add_tool_result(ToolResult {
            call_id: "c5".to_string(),
            output: "z".repeat(200_000), // huge
            success: true,
        });

        // Budget too small to fit the huge output — compaction MUST still leave
        // at least one non-system message.
        let (msgs, _stats) = build_messages(&conv, "sys", 10_000, "");
        let non_system = msgs.iter().filter(|m| !matches!(m.role, Role::System)).count();
        assert!(
            non_system > 0,
            "never return system-only result when messages exist — got msgs.len()={}",
            msgs.len()
        );
    }


    #[test]
    fn test_budgeted_emergency_restores_last_user_when_all_else_dropped() {
        // Even if every turn gets dropped by some path, the emergency fallback at
        // the bottom of to_provider_messages_budgeted should graft back the last
        // user message rather than return system-only.
        let mut conv = Conversation::new();
        conv.add_user_message("original question");
        // Add 20 turns of huge assistant+tool content to force aggressive drop
        for i in 0..20 {
            use crate::tool::{ToolCall, ToolResult};
            conv.add_assistant_tool_calls(Some(&format!("reasoning {}", i)), vec![ToolCall {
                id: format!("c{}", i),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            }]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", i),
                output: "y".repeat(10_000),
                success: true,
            });
        }

        let (msgs, _stats) = build_messages(&conv, "sys", 5_000, "");
        let has_user = msgs.iter().any(|m| matches!(m.role, Role::User));
        assert!(has_user, "last user message must always survive, got {} msgs", msgs.len());
    }


    #[test]
    fn test_cold_zone_compression() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 8 turns
        for turn in 0..8 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("c{}", turn),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(100),
                success: true,
            });
        }

        // Apply compression: remove first 9 messages (3 turns × 3 msgs each)
        conv.apply_compression(9, "User ran tasks 0, 1, 2 with bash.".to_string());

        // Cold zone should have 1 entry
        assert_eq!(conv.cold_summaries.len(), 1);
        // Messages should be reduced (first 3 turns removed)
        assert_eq!(conv.turn_tracker.turns.len(), 5); // 8 - 3

        // Budget check: cold zone should appear in output
        let (msgs, _stats) = build_messages(&conv, "sys", 100000, "");
        let has_cold = msgs.iter().any(|m| {
            m.text().map_or(false, |t| t.contains("Earlier conversation history"))
        });
        assert!(has_cold, "Cold zone summary should appear in output");
    }


    #[test]
    fn test_budgeted_drops_when_no_summary_and_over_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Create 3 turns with large content (no summaries)
        for turn in 0..3 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("c{}", turn),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call]);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(4000),
                success: true,
            });
        }

        // Small budget — force dropping
        let (msgs, stats) = build_messages(&conv, "sys", 2000, "");
        assert!(stats.dropped_tokens > 0, "Should drop turns when over budget");
        assert!(matches!(msgs[0].role, Role::System));
    }


    #[test]
    fn test_budgeted_preserves_message_order() {
        let mut conv = Conversation::new();
        conv.add_user_message("first");
        conv.messages.push(Message::new(Role::Assistant, "response 1"));
        conv.add_user_message("second");
        conv.messages.push(Message::new(Role::Assistant, "response 2"));
        conv.add_user_message("third");

        let (msgs, _stats) = build_messages(&conv, "sys", 100000, "");
        // system + 5 messages
        assert_eq!(msgs.len(), 6);
        assert_eq!(msgs[1].text(), Some("first"));
        assert_eq!(msgs[2].text(), Some("response 1"));
        assert_eq!(msgs[3].text(), Some("second"));
        assert_eq!(msgs[4].text(), Some("response 2"));
        assert_eq!(msgs[5].text(), Some("third"));
    }


    #[test]
    fn test_sanitize_removes_orphan_tool_results() {
        use crate::tool::ToolResult;
        let mut msgs = vec![
            Message::new(Role::System, "sys"),
            // Orphan tool result (no matching AssistantWithToolCalls)
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "orphan_1".to_string(),
                    output: "some output".to_string(),
                    success: true,
                }),
            },
            Message::new(Role::User, "hello"),
        ];
        sanitize_messages(&mut msgs);
        // Orphan should be removed, leaving System + User
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
    }


    #[test]
    fn test_sanitize_preserves_valid_pairs() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::System, "sys"),
            Message::new(Role::User, "do it"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "c1".to_string(),
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".to_string(),
                    output: "ok".to_string(),
                    success: true,
                }),
            },
        ];
        sanitize_messages(&mut msgs);
        // All 4 messages should be preserved (valid pair)
        assert_eq!(msgs.len(), 4);
    }
}
