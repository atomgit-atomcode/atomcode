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

use crate::conversation::message::{self, Message, MessageContent, Role};
use crate::conversation::{ContextStats, Conversation, KEEP_MESSAGES};

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
        return (
            vec![Message::new(Role::System, system_prompt)],
            ContextStats::default(),
        );
    }

    let system_msg = Message::new(Role::System, system_prompt);
    let system_tokens = system_msg.estimate_tokens();

    let turns = &conv.turn_tracker.turns;

    if turns.is_empty() {
        let remaining = token_budget.saturating_sub(system_tokens);
        return (
            build_messages_fallback(conv, system_msg, remaining),
            ContextStats::default(),
        );
    }

    let mut result = Vec::with_capacity(conv.messages.len() + 3);
    result.push(system_msg);

    // Inject cold zone summaries (if any)
    if !conv.cold_summaries.is_empty() {
        let cold_text = format!(
            "[Earlier conversation history ({} compression{})]\n{}",
            conv.cold_summaries.len(),
            if conv.cold_summaries.len() > 1 {
                "s"
            } else {
                ""
            },
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
        let last_turn_start = turns
            .get(last_turn_idx)
            .map(|t| t.start_idx)
            .unwrap_or(0)
            .min(conv.messages.len());

        // First pass: identify which turns to drop and extract their reasoning.
        // Loop bound `turns.len()-1` ensures we never touch the last turn.
        let mut drop_summaries: Vec<String> = Vec::new();
        let mut drop_count = 0usize;

        for ti in 0..turns.len().saturating_sub(1) {
            if dropped_tokens >= tokens_to_drop {
                break;
            }
            let turn = &turns[ti];
            let end = turn.end_idx().min(conv.messages.len());
            if turn.start_idx >= conv.messages.len() {
                continue;
            }

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
                    MessageContent::AssistantWithToolCalls {
                        text, tool_calls, ..
                    } => {
                        if let Some(t) = text {
                            let short: String = t.chars().take(150).collect();
                            if !short.trim().is_empty() {
                                parts.push(short);
                            }
                        }
                        let tools: Vec<&str> =
                            tool_calls.iter().map(|tc| tc.name.as_str()).collect();
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

            dropped_tokens += turn_msgs.iter().map(|m| m.estimate_tokens()).sum::<usize>();
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
                drop_summaries
                    .iter()
                    .enumerate()
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
            if turn.start_idx >= conv.messages.len() {
                continue;
            }
            let t: usize = conv.messages[turn.start_idx..end]
                .iter()
                .map(|m| m.estimate_tokens())
                .sum();
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

    // Microcompact: condense PRIOR-TURN ToolResults to one-line stubs.
    // Current turn (everything from last User message onward) is always
    // full-fidelity — see the microcompact() docstring for the
    // turn-aware boundary rationale.
    //
    // Threshold is 70% of the model's token budget converted to chars
    // (no cap). This means microcompact only fires when the
    // conversation is genuinely approaching the context window —
    // below 70% it's a no-op. The pre-5-8 design used a 100K-char cap
    // which triggered at ~20% of a 131K-token window, far too eagerly
    // (5-8 atomgr session: model echoed `HELLO_TEST_12345` to verify
    // it could see anything because earlier tool results in the same
    // turn had already been stubbed).
    let microcompact_threshold = (token_budget as u64 * 4 * 70 / 100) as usize;
    microcompact(&mut result, conv.messages.len(), microcompact_threshold);

    replace_stale_reads(&mut result);
    clean_message_pipeline(&mut result);

    // ── ABSOLUTE FLOOR (runs AFTER all cleanup, right before sent_tokens calc) ──
    // If compaction + cleanup somehow left us with only system messages, graft back
    // the last user message so the LLM has *something* to respond to. This is the
    // strictest possible invariant: whenever conv.messages is non-empty, the result
    // must contain at least one non-system message.
    let non_system_count = result
        .iter()
        .filter(|m| !matches!(m.role, Role::System))
        .count();
    if non_system_count == 0 {
        if let Some(last_user) =
            conv.messages.iter().rev().find(|m| {
                matches!(m.role, Role::User) && matches!(m.content, MessageContent::Text(..))
            })
        {
            result.push(Message::new(
                Role::System,
                "[Emergency: prior conversation was dropped during compaction. Only the latest user message is preserved.]"
            ));
            result.push(last_user.clone());
        }
    }

    // ── FINAL BYTE CEILING (last-line-of-defense) ──
    // microcompact protects the last 20 messages; the 80% drop cap at
    // line ~181 skips entirely when `cold_summaries` is populated
    // (legacy protection against a since-fixed pathology). That
    // leaves the recent window with no byte enforcement, so accumulated
    // mid-sized ToolResults can still blow the budget. Single
    // oldest-first forward pass: condense each ToolResult once only
    // (idempotent `condensed()` would otherwise spin), stop as soon
    // as the total fits under 80% of budget. The last 4 messages
    // (current turn's work) and Text / AssistantWithToolCalls are
    // never touched.
    let token_ceiling = token_budget.saturating_mul(80) / 100;
    let keep_tail = 4.min(result.len());
    let shrinkable_end = result.len().saturating_sub(keep_tail);
    // Build call_id → tool_name so `condensed` can pick the right
    // summarization strategy per tool (read_file → skeleton, others →
    // first-line). Without this, `condensed` would have had to guess
    // from output shape — a substring heuristic that false-positived
    // on bash outputs with `"  N| ..."` lines.
    let call_id_to_tool: std::collections::HashMap<String, String> = result
        .iter()
        .filter_map(|m| {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                Some(tool_calls.iter().map(|tc| (tc.id.clone(), tc.name.clone())))
            } else {
                None
            }
        })
        .flatten()
        .collect();
    for i in 1..shrinkable_end {
        let total: usize = result.iter().map(|m| m.estimate_tokens()).sum();
        if total <= token_ceiling {
            break;
        }
        let tool_name = match &result[i].content {
            MessageContent::ToolResult(r) => call_id_to_tool
                .get(&r.call_id)
                .map(|s| s.as_str())
                .unwrap_or(""),
            _ => continue,
        };
        let before = result[i].estimate_tokens();
        let condensed = result[i].condensed(tool_name);
        if condensed.estimate_tokens() < before {
            result[i] = condensed;
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

    let sent_tokens: usize = result
        .iter()
        .map(|m| m.estimate_tokens())
        .sum::<usize>()
        .saturating_sub(system_tokens);
    let msg_count = result.len();
    (
        result,
        ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens,
            total_messages: msg_count,
        },
    )
}

/// Reserved headroom for large windows (CC / Anthropic 200K territory)
/// where compaction can afford to leave a generous response + tool-result
/// runway. Mirrors CC's `AUTOCOMPACT_BUFFER_TOKENS`.
pub const AUTO_COMPACT_BUFFER_LARGE: usize = 13_000;

/// Reserved headroom for small/proxy-bound windows (typical self-hosted
/// GLM 65K). 5K leaves space for one streaming response + a round of
/// tool results without forcing compaction so early it shrinks the
/// usable session. Larger buffers (13K) on a 65K cap kick compaction at
/// 52K — wasting the 12K immediately above where users do real work.
pub const AUTO_COMPACT_BUFFER_SMALL: usize = 5_000;

/// Cutoff between "small" and "large" windows. 100K is the natural
/// dividing line: anything ≤ 100K is a self-hosted / proxy-bound
/// deployment that benefits from a tight buffer; anything > 100K is a
/// vendor offering (Anthropic 200K, etc.) where the wider buffer
/// matches CC's behaviour.
pub const AUTO_COMPACT_LARGE_WINDOW_FROM: usize = 100_000;

/// Compute the auto-compression trigger threshold for a given context
/// window. Returns the token total above which `needs_compression` fires.
///
/// Buffer scales with window size:
/// - ≤ 100K (proxy-bound): 5K buffer → 65K window → 60K trigger.
/// - > 100K (vendor large): 13K buffer → 200K window → 187K trigger.
/// - Either branch caps at `ctx_window / 4` so degenerate small windows
///   (8K Ollama) still land on a meaningful 6K threshold rather than
///   underflowing to 0.
pub fn auto_compact_threshold(token_budget: usize) -> usize {
    let raw_buffer = if token_budget > AUTO_COMPACT_LARGE_WINDOW_FROM {
        AUTO_COMPACT_BUFFER_LARGE
    } else {
        AUTO_COMPACT_BUFFER_SMALL
    };
    let buffer = raw_buffer.min(token_budget / 4);
    token_budget.saturating_sub(buffer)
}

/// Check if context needs compression.
///
/// Threshold derived from `auto_compact_threshold` — fires when fewer
/// than `buffer` tokens remain (5K for ≤100K windows, 13K for >100K).
/// Buffer scales with the deployment: self-hosted GLM at 65K trips
/// at 60K (4K runway is plenty for one round); Anthropic at 200K
/// trips at 187K, matching CC's behaviour.
///
/// The `messages.len() < 12` guard stays — needs a non-trivial backlog
/// before compression is worthwhile, and 1 user msg can produce 15+
/// messages so message count is the right unit.
pub fn needs_compression(
    conv: &Conversation,
    system_prompt_tokens: usize,
    token_budget: usize,
) -> bool {
    if conv.messages.len() < 12 {
        return false;
    }
    let total: usize = system_prompt_tokens
        + conv
            .messages
            .iter()
            .map(|m| m.estimate_tokens())
            .sum::<usize>();
    total > auto_compact_threshold(token_budget)
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

    let mut compress_end_idx = conv.messages.len() - KEEP_MESSAGES;

    // ── Pair-preserving snap ──
    // Anthropic API requires every `tool_result` to have its paired
    // `tool_use` in the same conversation. If the naive cut lands on a
    // ToolResult whose ATC lives in the drop range, the surviving range
    // begins with an orphan — `clean_message_pipeline` would silently
    // drop it and we'd lose the edit confirmation / tool output.
    //
    // Advance the cut forward past any trailing ToolResults so they
    // get dropped WITH their paired ATC (already in the drop range),
    // not kept as orphans. `compress_msgs` below uses the same index
    // so the summary captures these results too.
    while compress_end_idx < conv.messages.len() {
        match &conv.messages[compress_end_idx].content {
            message::MessageContent::ToolResult(_) | message::MessageContent::ToolResultRef(_) => {
                compress_end_idx += 1;
            }
            _ => break,
        }
    }

    // If snapping consumed all remaining messages, nothing to compress.
    if compress_end_idx >= conv.messages.len() {
        return (String::new(), 0);
    }

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
                if !s.starts_with('[') {
                    // skip system-injected messages
                    user_text = if s.chars().count() > 60 {
                        format!("{}...", s.chars().take(57).collect::<String>())
                    } else {
                        s.clone()
                    };
                }
            }
            (
                _,
                MessageContent::AssistantWithToolCalls {
                    text, tool_calls, ..
                },
            ) => {
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
                    let short = if let Ok(args) =
                        serde_json::from_str::<serde_json::Value>(&tc.arguments)
                    {
                        let fp = args.get("file_path").and_then(|v| v.as_str()).map(|p| {
                            std::path::Path::new(p)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| p.to_string())
                        });
                        match (tc.name.as_str(), fp) {
                            ("read_file", Some(f)) => format!("read {}", f),
                            ("edit_file", Some(f)) => format!("edit {}", f),
                            ("write_file", Some(f)) => format!("write {}", f),
                            ("grep", _) => {
                                let pat =
                                    args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
                                format!("grep({})", pat)
                            }
                            ("bash", _) => {
                                let cmd =
                                    args.get("command").and_then(|v| v.as_str()).unwrap_or("?");
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

    let tools_str = if tools.is_empty() {
        "no tools".to_string()
    } else {
        tools.join(", ")
    };

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
    format!(
        "- Turn {}: {}{}→ {}",
        turn_num, prefix, conclusion, tools_str
    )
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

/// Floor for collapse: outputs smaller than this are left alone.
/// Doubles as the idempotence guarantee — every stub we produce is
/// well under this size, so re-running compaction never re-stubs.
pub(crate) const MIN_COLLAPSE_SIZE: usize = 500;

/// Build the generic compaction stub used by both microcompact (render
/// time, ephemeral) and the conv-level Tier 1 (destructive). Tool name
/// comes from the model's own tool_calls so the framework adds zero
/// hardcoded tool knowledge — every tool gets the same shape.
///
/// **First-line picking**: skips `[elapsed: ...]` framework metadata.
/// `tool::bash` prepends `[elapsed: Xs, exit: N]\n<actual output>` to
/// every bash result (see bash.rs:540). 5-7 atomgr datalog showed all
/// 1704 bash stubs surfaced this metadata as `first:` content — model
/// got "1.9s, exit 101" instead of the actual error. Skipping to line 2
/// flips the stub from "exit code only" to "actual error / actual
/// output preview". Falls back to line 1 when there's no line 2
/// (single-line bash like `wc -l`). Non-bash tools (grep, edit_file,
/// web_fetch) don't have this prefix → unaffected.
///
/// **Hardcoding note**: matching `[elapsed:` is framework-internal
/// knowledge of our own bash tool's output format, not tech-stack
/// hardcoding (the prefix is the same regardless of cargo/npm/etc).
/// Same category as the `read_file` skip in microcompact.
pub(crate) fn build_compact_stub(tool_name: &str, output: &str, success: bool) -> String {
    let line_count = output.lines().count();
    let first_line: String = {
        let mut iter = output.lines();
        let l1 = iter.next().unwrap_or("(empty)");
        let chosen = if l1.starts_with("[elapsed:") {
            iter.next().unwrap_or(l1)
        } else {
            l1
        };
        chosen.chars().take(80).collect()
    };
    let status = if success { "ok" } else { "FAILED" };
    format!(
        "[{} {}: {} lines, first: {}]",
        tool_name, status, line_count, first_line,
    )
}

/// Build a `call_id -> tool_name` lookup from a slice of messages. The
/// `MessageContent::AssistantWithToolCalls` variant carries the model's
/// own tool name; this is what we surface in stubs.
fn build_call_id_to_tool_map(
    msgs: &[Message],
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for msg in msgs {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                map.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }
    map
}

/// Conv-level Tier 1 compaction. Replaces tool_result bodies in turns
/// older than `keep_recent_turns` with the same generic stub used by
/// microcompact. This is the destructive counterpart: microcompact runs
/// every render and is ephemeral (only mutates the rendered Vec); this
/// runs from the agent emergency path and permanently shrinks
/// `conv.messages` so the next `needs_compression` check sees the
/// freed budget.
///
/// Idempotent: stubs already in place are smaller than MIN_COLLAPSE_SIZE
/// and skip the rewrite.
pub(crate) fn compact_old_tool_results_in_place(
    conv: &mut crate::conversation::Conversation,
    keep_recent_turns: usize,
) {
    let turns = &conv.turn_tracker.turns;
    if turns.len() <= keep_recent_turns {
        return;
    }
    let cutoff_turn = turns.len() - keep_recent_turns;
    let cutoff_msg = turns[cutoff_turn].start_idx.min(conv.messages.len());

    let call_id_to_tool = build_call_id_to_tool_map(&conv.messages);

    for i in 0..cutoff_msg {
        let MessageContent::ToolResult(ref tr) = conv.messages[i].content else {
            continue;
        };
        if tr.output.len() <= MIN_COLLAPSE_SIZE {
            continue;
        }
        let tool_name = call_id_to_tool
            .get(&tr.call_id)
            .map(|s| s.as_str())
            .unwrap_or("tool");
        let summary = build_compact_stub(tool_name, &tr.output, tr.success);
        conv.messages[i].content = MessageContent::ToolResult(crate::tool::ToolResult {
            call_id: tr.call_id.clone(),
            output: summary,
            success: tr.success,
        });
    }
}

/// Microcompact: condense **prior-turn** `ToolResult` messages to one-line
/// semantic summaries. Zero LLM calls — purely mechanical compression.
///
/// **Turn-aware boundary (5-8 redesign).** Earlier versions used a
/// fixed `OTHER_KEEP = 20` last-messages window. That window slid every
/// LLM round, so within ONE user turn the model's earlier tool results
/// got progressively stubbed as the model emitted more tool calls —
/// the "model echoes HELLO_TEST_12345 to verify it can see anything"
/// 5-8 atomgr session was caused by this. Now we anchor on the last
/// `Role::User` message in the rendered Vec: everything from that
/// message onward IS the current turn and stays full-fidelity; only
/// strictly older content is eligible for stubbing.
///
/// **Threshold (5-8 redesign).** Earlier capped at 100K chars (~25K
/// tokens) → triggered at ~20% of a 131K-token window, way too eager.
/// Now `threshold_chars = 70% × token_budget × 4` (uncapped) so
/// microcompact only fires when the conversation is genuinely close
/// to filling the model's window. Below 70% it's a no-op.
///
/// **Stub format.** `[<tool_name> <ok|FAILED>: N lines, first: <80c>]`.
/// Tool name comes from the model's own `tool_calls.name` (no
/// `match tool_name { "bash" => ... }` framework branches). `read_file`
/// is exempted by hardcoded skip — see in-line comment for rationale.
fn microcompact(msgs: &mut Vec<Message>, _total_msg_count: usize, threshold_chars: usize) {
    let total_chars: usize = msgs
        .iter()
        .map(|m| match &m.content {
            MessageContent::ToolResult(r) => r.output.len(),
            MessageContent::Text(t) => t.len(),
            _ => 100,
        })
        .sum();
    if total_chars < threshold_chars {
        return;
    }

    // Anchor on the last User message — everything after it is the
    // ACTIVE turn and must stay full. If no User message (cold start
    // / system-only), there's nothing to compress yet.
    let current_turn_start = match msgs
        .iter()
        .rposition(|m| matches!(m.role, Role::User))
    {
        Some(i) => i,
        None => return,
    };

    let cold_msgs = msgs
        .iter()
        .position(|m| !matches!(m.role, Role::System))
        .unwrap_or(0);

    if cold_msgs >= current_turn_start {
        return; // nothing between system and current turn
    }

    let call_id_to_tool = build_call_id_to_tool_map(msgs);

    for i in cold_msgs..current_turn_start {
        let MessageContent::ToolResult(ref r) = msgs[i].content else {
            continue;
        };

        if r.output.len() <= MIN_COLLAPSE_SIZE {
            continue;
        }

        let tool_name = call_id_to_tool
            .get(&r.call_id)
            .map(|s| s.as_str())
            .unwrap_or("tool");

        // read_file 永远不被 microcompact 压缩。stub 给模型的
        // `first: 205| pub async fn dynamic_connect(` 信息会制造"伪自信"
        // ——模型以为还记得函数体就直接 edit，结果反复修同一个文件
        // (5-7 atomgr datalog T22-T29 实证 6 turn 反复修补)。保留全文
        // 让模型在 edit 系列 turn 里始终看到最新代码。
        // D3 FileStore 已经处理 re-read 的 disk-side 成本；prompt-side
        // 多花 5-10% token 换"模型不丢上下文"，是值得的交易。
        //
        // 关于硬编码: 这里直接字符串比较 "read_file"，而非工具自声明
        // (e.g. trait fn microcompact_eligible)。妥协理由：
        // (a) "read_file" 是框架自家工具名常量，不是 cargo/npm/pytest
        //     这类技术栈关键字，不违反"框架对技术栈中立"的项目铁律；
        // (b) 改成 trait 方法需要把 ToolRegistry 引用穿进 render 层，
        //     渲染路径调用面增大，收益不抵成本；
        // (c) 仅此一处，未来如有第二个工具也要豁免，再重构成 trait。
        if tool_name == "read_file" {
            continue;
        }

        let summary = build_compact_stub(tool_name, &r.output, r.success);

        msgs[i].content = MessageContent::ToolResult(crate::tool::ToolResult {
            call_id: r.call_id.clone(),
            output: summary,
            success: r.success,
        });
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
    let mut call_id_to_read: std::collections::HashMap<String, ReadInfo> =
        std::collections::HashMap::new();
    let mut edit_call_to_file: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut edited_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in msgs.iter() {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                    let file_path = args
                        .get("file_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if tc.name == "read_file" && !file_path.is_empty() {
                        let offset = args
                            .get("offset")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize);
                        let limit = args
                            .get("limit")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as usize);
                        call_id_to_read.insert(
                            tc.id.clone(),
                            ReadInfo {
                                file_path: file_path.clone(),
                                offset,
                                limit,
                            },
                        );
                    }
                    if matches!(tc.name.as_str(), "edit_file" | "write_file" | "create_file")
                        && !file_path.is_empty()
                    {
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
                if !edited_files.contains(&info.file_path) {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&info.file_path) {
                    let all_lines: Vec<&str> = content.lines().collect();
                    let total = all_lines.len();

                    if info.offset.is_some() || info.limit.is_some() {
                        let start = info.offset.unwrap_or(1).max(1) - 1;
                        let start = start.min(total);
                        let end = info.limit.map(|l| (start + l).min(total)).unwrap_or(total);
                        let display: String = all_lines[start..end]
                            .iter()
                            .enumerate()
                            .map(|(i, l)| format!("{:>4}| {}", start + i + 1, l))
                            .collect::<Vec<_>>()
                            .join("\n");
                        r.output = display;
                    } else if total <= 300 {
                        r.output = all_lines
                            .iter()
                            .enumerate()
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
///
/// Drops three kinds of broken state:
///
/// 1. **Orphan ToolResult** — appears outside any `expecting` window
///    (no preceding AssistantWithToolCalls awaiting it). Removed solo.
/// 2. **Mid-conversation under-paired AssistantWithToolCalls** — has N
///    tool_calls but a Text / MultiPart / next ATC arrives before all N
///    ToolResults have been seen. The unsatisfied ATC AND any partial
///    ToolResults already paired with it are removed together. This is
///    the path that triggers DeepSeek's `insufficient tool messages
///    following tool_calls message` 400 — the strictest providers
///    require the wire-level invariant `len(asst.tool_calls) ==
///    len(following tool messages)` to hold for every ATC, not just the
///    most recent one.
/// 3. **Trailing under-paired AssistantWithToolCalls** — same as (2)
///    but the conversation ends mid-pairing. Handled by the rev-scan
///    after the main loop.
fn sanitize_messages(msgs: &mut Vec<Message>) {
    let mut to_remove: Vec<usize> = Vec::new();
    let mut expecting_tool_results = 0usize;
    // Track the most recent ATC and the ToolResult indices already
    // paired with it. On a boundary (Text / MultiPart / next ATC) with
    // `expecting > 0`, both the ATC and its partial results are dropped.
    let mut current_atc_idx: Option<usize> = None;
    let mut current_atc_results: Vec<usize> = Vec::new();

    for i in 0..msgs.len() {
        match &msgs[i].content {
            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                if expecting_tool_results > 0 {
                    expecting_tool_results -= 1;
                    current_atc_results.push(i);
                } else {
                    to_remove.push(i);
                }
            }
            MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                if expecting_tool_results > 0 {
                    if let Some(idx) = current_atc_idx {
                        to_remove.push(idx);
                    }
                    to_remove.extend(current_atc_results.drain(..));
                } else {
                    current_atc_results.clear();
                }
                expecting_tool_results = tool_calls.len();
                current_atc_idx = Some(i);
            }
            MessageContent::Text(_) | MessageContent::MultiPart { .. } => {
                if expecting_tool_results > 0 {
                    if let Some(idx) = current_atc_idx {
                        to_remove.push(idx);
                    }
                    to_remove.extend(current_atc_results.drain(..));
                } else {
                    current_atc_results.clear();
                }
                expecting_tool_results = 0;
                current_atc_idx = None;
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
/// - Consecutive system messages (merge into one) — MiniMax-M2.7 rejects
///   adjacent `system` turns with `2013 invalid chat setting`; the
///   post-compression layout (orig system + cold-zone + drop-digest) is
///   the trigger.
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

    // 5. Merge consecutive system messages into one. After compression the
    // wire layout is `system(orig) + system(cold-zone) [+ system(drop-digest)]`,
    // which MiniMax-M2.7's chat-setting validator rejects (empty stream then
    // 400 / 2013). Blank line between blocks preserves visual separation.
    let mut i = 1;
    while i < msgs.len() {
        if msgs[i].role == Role::System && msgs[i - 1].role == Role::System {
            if let (MessageContent::Text(prev), MessageContent::Text(curr)) =
                (&msgs[i - 1].content, &msgs[i].content)
            {
                let merged = format!("{}\n\n{}", prev, curr);
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
    use crate::conversation::message::{Message, Role};
    use crate::conversation::Conversation;

    #[test]
    fn apply_model_directives_noop_for_generic_model() {
        // gpt / claude / gemini 等模型不触发任何指令 — 原 prompt 原样返回。
        let out = apply_model_directives("SYS", "gpt-4o");
        assert_eq!(out, "SYS");
        let out = apply_model_directives("SYS", "claude-opus-4-7");
        assert_eq!(out, "SYS");
    }

    #[test]
    fn auto_compact_threshold_large_window_uses_large_buffer() {
        // > 100K → 13K buffer (Anthropic / CC territory). 200K - 13K = 187K.
        assert_eq!(auto_compact_threshold(200_000), 187_000);
        // 131K → boundary above the 100K cutoff, also gets 13K buffer.
        assert_eq!(auto_compact_threshold(131_072), 118_072);
    }

    #[test]
    fn auto_compact_threshold_small_window_uses_small_buffer() {
        // ≤ 100K → 5K buffer (proxy-bound deployments). 65K - 5K = 60K
        // — exactly the sweet spot for a 65K self-hosted GLM cap:
        // compaction kicks in 5K below the proxy hard wall, leaving
        // a runway for one streaming response without forcing
        // pre-emptive compaction so early it shrinks the usable
        // session.
        assert_eq!(auto_compact_threshold(65_000), 60_000);
        // 100K is the boundary — still small-buffer (the cutoff is
        // strictly greater-than).
        assert_eq!(auto_compact_threshold(100_000), 95_000);
        // Just over 100K trips into large-buffer territory.
        assert_eq!(auto_compact_threshold(101_000), 88_000);
    }

    #[test]
    fn auto_compact_threshold_tiny_window_caps_at_quarter() {
        // 8K Ollama: 5K buffer would still leave only 3K usable, but
        // window/4 = 2K caps the buffer below 5K → 6K threshold (~75%
        // of window). Scales the buffer when the window is too small
        // for the small-buffer constant.
        assert_eq!(auto_compact_threshold(8_000), 6_000);
        assert_eq!(auto_compact_threshold(16_000), 12_000);
        // At 20K the small-buffer constant (5K) lands at exactly
        // window/4, so 5K applies straight: 20K - 5K = 15K.
        assert_eq!(auto_compact_threshold(20_000), 15_000);
    }

    #[test]
    fn auto_compact_threshold_handles_degenerate_window() {
        // ctx_window == 0 happens transiently before the provider config
        // loads; saturating_sub keeps it from panicking. Threshold is 0,
        // so any non-empty conversation trips the gate — caller's
        // `messages.len() < 12` check still gates the actual fire.
        assert_eq!(auto_compact_threshold(0), 0);
    }

    #[test]
    fn needs_compression_fires_at_absolute_headroom_not_percentage() {
        // Reproduces the user's debug confusion: under the prior formula
        // a 131K window's threshold was `min(131K * 50%, 50K) = 50K` —
        // compression fired at 38% of window, leaving 81K of phantom
        // "available" headroom that wasn't actually used. The new
        // formula fires at 118K (90% of window), matching the user's
        // intuition of "fire when ~13K headroom remains".
        //
        // Test fixture: 15 alternating User/Assistant messages so the
        // 12-message guard passes (`add_user_message` merges
        // consecutive User msgs, which would collapse 15 calls into 1).
        let mut conv = Conversation::new();
        for i in 0..8 {
            conv.messages.push(Message::new(Role::User, format!("u{}", i)));
            conv.messages.push(Message::new(Role::Assistant, format!("a{}", i)));
        }
        assert_eq!(conv.messages.len(), 16);
        assert!(!needs_compression(&conv, 0, 131_072));

        // 500K bytes ≈ 125K tokens (byte / 4) → exceeds 118K threshold.
        conv.messages
            .push(Message::new(Role::User, "x".repeat(500_000)));
        assert!(needs_compression(&conv, 0, 131_072));
    }

    #[test]
    fn tool_result_ref_token_estimate_uses_summary_not_byte_size() {
        // Pre-fix bug: ToolResultRef estimated from the full original
        // content size (could be 50K+ for a large file read), but at
        // send time only `r.summary` (a short string) was actually
        // serialised. The estimator overcounted by 5-50× on
        // externalised results, pushing compression to fire on phantom
        // budget pressure.
        use crate::conversation::message::MessageContent;
        use crate::tool::result_store::ToolResultRef;

        let big_ref = ToolResultRef {
            call_id: "call_1".into(),
            hash: "deadbeef".into(),
            summary: "hello".into(), // 5 bytes
            byte_size: 200_000,      // pretend the disk-cached blob is 200KB
            success: true,
        };
        let msg = Message {
            role: Role::User,
            content: MessageContent::ToolResultRef(big_ref),
        };
        // (5 + 10) / 4 + 4 = 7. Pre-fix this was (200000 + 10) / 4 + 4 = 50006.
        assert!(
            msg.estimate_tokens() < 20,
            "expected estimate to track summary size, got {}",
            msg.estimate_tokens()
        );
    }

    #[test]
    fn apply_model_directives_cn_lock_for_cjk_tier() {
        for id in ["qwen3-max", "deepseek-v3", "kimi-k2"] {
            let out = apply_model_directives("SYS", id);
            assert!(
                out.contains("用户可见的输出请用中文"),
                "model {id} missing CN lock"
            );
            assert!(
                !out.contains("THINKING 简洁纪律"),
                "model {id} got MiniMax directive erroneously"
            );
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
        conv.messages
            .push(Message::new(Role::Assistant, "hi there"));
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
            conv.add_assistant_tool_calls(None, vec![call], None);
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
                conv.add_assistant_tool_calls(None, vec![call], None);
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
        assert!(
            stats.dropped_tokens > 0,
            "Some turns should have been dropped"
        );
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
        conv.add_assistant_tool_calls(Some("running..."), vec![call], None);
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
            conv.add_assistant_tool_calls(Some("ok"), vec![call], None);
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
        conv.add_assistant_tool_calls(Some("finding..."), vec![call], None);
        conv.add_tool_result(ToolResult {
            call_id: "c5".to_string(),
            output: "z".repeat(200_000), // huge
            success: true,
        });

        // Budget too small to fit the huge output — compaction MUST still leave
        // at least one non-system message.
        let (msgs, _stats) = build_messages(&conv, "sys", 10_000, "");
        let non_system = msgs
            .iter()
            .filter(|m| !matches!(m.role, Role::System))
            .count();
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
            conv.add_assistant_tool_calls(
                Some(&format!("reasoning {}", i)),
                vec![ToolCall {
                    id: format!("c{}", i),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", i),
                output: "y".repeat(10_000),
                success: true,
            });
        }

        let (msgs, _stats) = build_messages(&conv, "sys", 5_000, "");
        let has_user = msgs.iter().any(|m| matches!(m.role, Role::User));
        assert!(
            has_user,
            "last user message must always survive, got {} msgs",
            msgs.len()
        );
    }

    #[test]
    fn microcompact_uses_generic_format_with_tool_label_from_call_id() {
        // microcompact emits a single generic format:
        // `[<tool> <ok|FAILED>: N lines, first: <line>]`. Tool label comes
        // from the model's own `tool_calls.name`, not a `match` on
        // hardcoded strings — passes the project's tech-stack-neutrality
        // rule. Bash, grep, glob, and unknown-tool calls all flow
        // through the same template.
        //
        // read_file is exempted (5-7 atomgr datalog showed weak models
        // build "伪自信" from `first: 205| pub async fn dynamic_connect(`
        // and edit blind). Skip behavior is covered by
        // `microcompact_skips_read_file_to_preserve_long_session_context`.
        //
        // Calls `microcompact` directly so the test isolates stub format
        // from the rendering pipeline's drop / compression logic.
        use crate::tool::{ToolCall, ToolResult};

        let mut msgs: Vec<Message> = vec![Message::new(Role::System, "sys")];
        msgs.push(Message::new(Role::User, "explore"));

        let kinds = [
            ("c_bok", "bash", true),
            ("c_bfail", "bash", false),
            ("c_grep", "grep", true),
            ("c_mcp", "mcp_remote.exec", true),
        ];
        for (id, name, success) in &kinds {
            msgs.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
            });
            msgs.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: (*id).to_string(),
                    output: format!("first line for {}\n{}", name, "x".repeat(4_000)),
                    success: *success,
                }),
            });
        }

        // Anchor the next turn so the prior tool results above are
        // eligible for compaction (turn-aware boundary).
        msgs.push(Message::new(Role::User, "now what"));

        let n = msgs.len();
        // Low threshold so microcompact fires deterministically.
        microcompact(&mut msgs, n, 1_000);

        let find_by_id = |id: &str| -> Option<String> {
            msgs.iter().find_map(|m| {
                if let MessageContent::ToolResult(r) = &m.content {
                    if r.call_id == id {
                        return Some(r.output.clone());
                    }
                }
                None
            })
        };

        // bash (success) → compacted with `bash ok: ...` label.
        let bok = find_by_id("c_bok").expect("c_bok must survive");
        assert!(
            bok.starts_with("[bash ok: ") && bok.contains("first: "),
            "bash success format mismatch: {}",
            bok
        );

        // bash (failure) → `bash FAILED: ...` label preserves the
        // success/fail axis the model needs for retry reasoning.
        let bfail = find_by_id("c_bfail").expect("c_bfail must survive");
        assert!(
            bfail.starts_with("[bash FAILED: ") && bfail.contains("first: "),
            "bash failure format mismatch: {}",
            bfail
        );

        // grep and an unknown tool name use the same template — no
        // special-case match arms inside microcompact (read_file is
        // exempted; see `microcompact_skips_read_file_*`).
        for (id, expected_label) in [
            ("c_grep", "grep"),
            ("c_mcp", "mcp_remote.exec"),
        ] {
            let body = find_by_id(id).unwrap_or_else(|| panic!("{} must survive", id));
            assert!(
                body.starts_with(&format!("[{} ok: ", expected_label)),
                "{} expected generic `[{} ok: ...]` format, got: {}",
                id,
                expected_label,
                body
            );
            assert!(
                body.contains("first: first line for"),
                "{} should preserve first-line snippet, got: {}",
                id,
                body
            );
        }
    }

    /// 5-7 atomgr datalog (build 942b615): 1704/1704 bash stubs surfaced
    /// `first: [elapsed: Xs, exit: N]` — framework metadata, zero signal.
    /// Stub now skips that line and shows line 2 (the real output / real
    /// error). Failed bash retry decisions go from "exit 101 of unknown
    /// origin" to "actual error: ...".
    #[test]
    fn build_compact_stub_skips_bash_elapsed_metadata() {
        let bash_failure = "[elapsed: 1.9s, exit: 101]\nerror: cannot find type `Foo` in this scope";
        let stub = build_compact_stub("bash", bash_failure, false);
        assert!(
            stub.contains("error: cannot find type"),
            "bash stub must surface the actual error, not the elapsed metadata: {}",
            stub
        );
        assert!(
            !stub.contains("first: [elapsed:"),
            "bash stub first-line must skip the elapsed metadata: {}",
            stub
        );
    }

    /// Single-line bash (`wc -l`, `echo $?`, etc.) has no line 2 to fall
    /// through to. Stub must use whatever line 1 is rather than blanking.
    #[test]
    fn build_compact_stub_falls_back_to_line1_when_only_one_line() {
        let one_liner = "42";
        let stub = build_compact_stub("bash", one_liner, true);
        assert!(stub.contains("first: 42"), "got: {}", stub);
    }

    /// `[elapsed:` skip is bash-only by virtue of the prefix being unique
    /// to our bash tool. grep / edit_file / web_fetch outputs do NOT
    /// start with `[elapsed:` so they hit the normal line-1 path. This
    /// test pins that the skip doesn't accidentally eat the first useful
    /// line of those tools.
    #[test]
    fn build_compact_stub_unaffected_for_non_bash_tools() {
        let grep = "src/foo.rs:42:    fn bar() {}\nsrc/baz.rs:10:    fn baz()";
        let stub = build_compact_stub("grep", grep, true);
        assert!(
            stub.contains("first: src/foo.rs:42:"),
            "grep stub must keep line 1 intact: {}",
            stub
        );

        let edit = "Edited /path/to/file.rs (-3 +5 lines).";
        let stub = build_compact_stub("edit_file", edit, true);
        assert!(stub.contains("first: Edited /path"), "got: {}", stub);
    }

    /// 5-7 atomgr datalog (atomgr-2d99b47d/2026-05-07_00-28-34): T22-T29
    /// reveal weak models develop "伪自信" when read_file is stubbed —
    /// `[read_file ok: 115 lines, first: 205| pub async fn dynamic_connect(]`
    /// gives just enough surface (line number + function name) for the
    /// model to think it remembers the body, then it edits blind. Result:
    /// 6 turns of patch-and-repatch the same file. Keeping read_file
    /// FULL preserves attention on the actual code; D3 FileStore handles
    /// the disk-side cost of re-reads transparently.
    #[test]
    fn microcompact_skips_read_file_to_preserve_long_session_context() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("explore");

        // One read_file call with a large body — would normally be
        // compacted under the generic path.
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "c_read".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
            None,
        );
        let read_body = format!("first line of read\n{}", "x".repeat(5_000));
        conv.add_tool_result(ToolResult {
            call_id: "c_read".into(),
            output: read_body.clone(),
            success: true,
        });

        // Pad with bash so total_chars crosses microcompact's
        // threshold. Use a small budget (8K tokens → 22_400 char
        // threshold) so the 30 padding bashes + the read_file body
        // (~125K chars total) reliably triggers microcompact.
        for i in 0..30 {
            let id = format!("c_pad{}", i);
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall {
                    id: id.clone(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(4_000)),
                success: true,
            });
        }
        conv.add_user_message("now what");

        // 40K budget → 112K char threshold. Payload (read body 5K +
        // 30 × 4K padding ≈ 125K chars / ~31K tokens) crosses
        // threshold but fits budget without triggering build_messages
        // pre-microcompact drops.
        let (msgs, _) = build_messages(&conv, "sys", 40_000, "");

        // Locate the read_file ToolResult in the rendered messages.
        let body = msgs
            .iter()
            .find_map(|m| {
                if let MessageContent::ToolResult(r) = &m.content {
                    if r.call_id == "c_read" {
                        return Some(r.output.clone());
                    }
                }
                None
            })
            .expect("c_read must survive in rendered messages");

        // Read body must remain FULL — never replaced with the generic
        // `[read_file ok: ... first: ...]` stub.
        assert!(
            !body.starts_with("[read_file "),
            "read_file got compacted (伪自信 risk): {}",
            &body[..body.len().min(200)]
        );
        assert_eq!(
            body.len(),
            read_body.len(),
            "read_file body length must equal original (uncompacted)"
        );
        assert!(
            body.contains("first line of read"),
            "first line lost: {}",
            &body[..body.len().min(200)]
        );

        // Sanity: bash padding ToolResults DID get compacted — confirms
        // the threshold actually triggered, the test isn't passing
        // because microcompact was a no-op.
        let any_bash_compacted = msgs.iter().any(|m| {
            if let MessageContent::ToolResult(r) = &m.content {
                r.output.starts_with("[bash ok: ")
            } else {
                false
            }
        });
        assert!(
            any_bash_compacted,
            "bash padding should have been compacted; if not, the \
             threshold isn't actually triggering and read_file passing \
             through is a false positive"
        );
    }

    /// 5-8 atomgr session bug — microcompact was stubbing the CURRENT
    /// turn's earlier tool results, leading the model to echo
    /// `HELLO_TEST_12345` self-checks because mid-turn it could no
    /// longer see what it had just done. The fix: anchor on the last
    /// `Role::User` message in the rendered Vec — everything from
    /// that message onward is the active turn and stays full-fidelity.
    /// Only strictly older content is eligible for stubbing.
    ///
    /// Calls `microcompact` directly (not through `build_messages`) so
    /// the test isolates the boundary logic from the rendering
    /// pipeline's drop / token-budget handling.
    #[test]
    fn microcompact_preserves_current_turn_in_full() {
        use crate::tool::{ToolCall, ToolResult};

        // Build a Vec<Message> manually with a clear turn boundary:
        // System | User#1 | (Asst tool_calls + Tool results)×15 | User#2 | (Asst+Tool)×10
        // Last User is User#2 → current turn is everything after it.
        let mut msgs: Vec<Message> = vec![Message::new(Role::System, "sys")];

        // ── PRIOR turn ────────────────────────────────────────
        msgs.push(Message::new(Role::User, "first task"));
        for i in 0..15 {
            let id = format!("prior_{}", i);
            msgs.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: id.clone(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
            });
            msgs.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: id,
                    output: format!("[elapsed: 0.0s, exit: 0]\n{}", "p".repeat(4_000)),
                    success: true,
                }),
            });
        }

        // ── CURRENT turn (must stay full) ──────────────────────
        msgs.push(Message::new(Role::User, "second task"));
        for i in 0..10 {
            let id = format!("current_{}", i);
            msgs.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: id.clone(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
            });
            msgs.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: id,
                    output: format!("[elapsed: 0.0s, exit: 0]\n{}", "c".repeat(4_000)),
                    success: true,
                }),
            });
        }

        let total_chars: usize = msgs
            .iter()
            .map(|m| match &m.content {
                MessageContent::ToolResult(r) => r.output.len(),
                MessageContent::Text(t) => t.len(),
                _ => 100,
            })
            .sum();
        // Set threshold low so microcompact fires deterministically.
        let n = msgs.len();
        microcompact(&mut msgs, n, 1_000);

        let collect = |prefix: &str| -> Vec<(String, String)> {
            msgs.iter()
                .filter_map(|m| match &m.content {
                    MessageContent::ToolResult(r) if r.call_id.starts_with(prefix) => {
                        Some((r.call_id.clone(), r.output.clone()))
                    }
                    _ => None,
                })
                .collect()
        };

        // PRIOR turn: every tool result must be stubbed.
        let prior = collect("prior_");
        assert_eq!(prior.len(), 15, "expected 15 prior tool results");
        for (cid, body) in &prior {
            assert!(
                body.starts_with("[bash "),
                "prior turn `{}` must be stubbed; got body of len={} starting {:?}\n\
                 (total_chars before microcompact was {})",
                cid,
                body.len(),
                &body[..body.len().min(80)],
                total_chars
            );
            assert!(
                body.len() < 200,
                "prior stub should be < 200 bytes, got {}",
                body.len()
            );
        }

        // CURRENT turn: every tool result must remain FULL.
        let current = collect("current_");
        assert_eq!(current.len(), 10, "expected 10 current tool results");
        for (cid, body) in &current {
            assert!(
                !body.starts_with("[bash "),
                "current turn `{}` must NOT be stubbed (turn-aware preservation): \
                 got {:?}",
                cid,
                &body[..body.len().min(80)]
            );
            assert!(
                body.len() > 4_000,
                "current tool result must keep its full payload (>4K chars), \
                 got {} bytes",
                body.len()
            );
        }
    }

    /// Running compaction twice MUST be idempotent — the upgraded
    /// microcompact's `len <= MIN_COLLAPSE_SIZE` guard ensures that
    /// once a stub is in place, the next pass sees a < 500-char
    /// result and skips it rather than re-stubbing into a less-useful
    /// "[older tool result collapsed (60 chars dropped)]" form
    /// (the bug pattern from before this unification).
    #[test]
    fn microcompact_is_idempotent_no_double_stub() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("trigger");
        for i in 0..30 {
            let id = format!("c{}", i);
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall {
                    id: id.clone(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("first line\n{}", "x".repeat(4_000)),
                success: true,
            });
        }
        conv.add_user_message("done");

        let (msgs1, _) = build_messages(&conv, "sys", 131_072, "");
        let (msgs2, _) = build_messages(&conv, "sys", 131_072, "");

        // Compaction is pure over (conv, threshold) — two passes must
        // produce byte-identical compacted bodies, no degradation.
        let collect_tr = |m: &[Message]| -> Vec<String> {
            m.iter()
                .filter_map(|m| {
                    if let MessageContent::ToolResult(r) = &m.content {
                        Some(r.output.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };
        assert_eq!(collect_tr(&msgs1), collect_tr(&msgs2));
        // And concretely: every stub stays in `[bash ok: ...]` form,
        // never devolves into `[older tool result collapsed ...]`.
        for body in collect_tr(&msgs1) {
            if body.starts_with("[bash") {
                assert!(
                    body.contains("first: "),
                    "stub lost its first-line slot: {}",
                    body
                );
            }
        }
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
            conv.add_assistant_tool_calls(Some("ok"), vec![call], None);
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
            m.text()
                .map_or(false, |t| t.contains("Earlier conversation history"))
        });
        assert!(has_cold, "Cold zone summary should appear in output");
    }

    /// Regression: MiniMax-M2.7 returns empty content + 400 (`2013 invalid
    /// chat setting`) when the request contains adjacent `system` messages.
    /// Post-compression layout used to ship `system(orig) + system(cold-zone)`
    /// straight to the wire — `clean_message_pipeline` now coalesces them.
    #[test]
    fn test_no_consecutive_system_messages_after_compression() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        for turn in 0..8 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("c{}", turn),
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            };
            conv.add_assistant_tool_calls(Some("ok"), vec![call], None);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(100),
                success: true,
            });
        }

        conv.apply_compression(9, "User ran tasks 0, 1, 2 with bash.".to_string());
        assert_eq!(conv.cold_summaries.len(), 1);

        let (msgs, _stats) = build_messages(&conv, "you are atomcode", 100_000, "");

        for pair in msgs.windows(2) {
            assert!(
                !(pair[0].role == Role::System && pair[1].role == Role::System),
                "consecutive system messages found at the wire boundary"
            );
        }

        // The merged system message must still carry both the original
        // prompt and the cold-zone summary so the model retains context.
        let merged = msgs
            .iter()
            .find(|m| matches!(m.role, Role::System))
            .and_then(|m| m.text())
            .expect("at least one system message");
        assert!(
            merged.contains("you are atomcode"),
            "merged system must keep original prompt"
        );
        assert!(
            merged.contains("Earlier conversation history"),
            "merged system must keep cold-zone summary"
        );
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
            conv.add_assistant_tool_calls(Some("ok"), vec![call], None);
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(4000),
                success: true,
            });
        }

        // Small budget — force dropping
        let (msgs, stats) = build_messages(&conv, "sys", 2000, "");
        assert!(
            stats.dropped_tokens > 0,
            "Should drop turns when over budget"
        );
        assert!(matches!(msgs[0].role, Role::System));
    }

    /// Bug b regression: after compression has run once, `cold_summaries`
    /// is non-empty, which disables the 80% drop cap above (legacy
    /// pathology guard). Microcompact still skips the last
    /// `OTHER_KEEP=20` messages. That leaves the recent window with no
    /// byte enforcement, so many mid-sized ToolResults can blow budget.
    /// The final post-cleanup byte ceiling must condense oldest
    /// ToolResults in `result` until total estimated tokens fit under
    /// 80% of the budget.
    #[test]
    fn test_final_byte_ceiling_condenses_oversized_recent_toolresults() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        // Mark that a prior compression already ran — cold_summaries
        // non-empty is the precondition that disables the earlier cap.
        conv.cold_summaries.push("earlier task summary".to_string());

        // 20 turns, each with a 6K-char bash result. microcompact's
        // OTHER_KEEP=20 leaves the trailing 20 messages (≈ last 6-7 turns)
        // untouched — those alone sum to > 36K chars ≈ 9K+ est tokens,
        // which exceeds the 80% ceiling of the chosen budget.
        for turn in 0..20 {
            conv.add_user_message(&format!("task {}", turn));
            conv.add_assistant_tool_calls(
                Some("ok"),
                vec![ToolCall {
                    id: format!("c{}", turn),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: format!("c{}", turn),
                output: "x".repeat(6000),
                success: true,
            });
        }

        // token_budget = 10K tokens → ceiling = 8K tokens.
        let (msgs, _stats) = build_messages(&conv, "sys", 10_000, "");
        let total_tokens: usize = msgs.iter().map(|m| m.estimate_tokens()).sum();
        assert!(
            total_tokens <= 8_000,
            "Total estimated tokens {} exceeded 80% ceiling 8000 — \
             final byte ceiling did not run",
            total_tokens,
        );
        // The newest turn's tool result must survive in full (not condensed).
        let newest_still_full = msgs
            .iter()
            .any(|m| m.text().map_or(false, |t| t.contains(&"x".repeat(100))));
        assert!(
            newest_still_full,
            "Newest turn's full-size tool result must be preserved",
        );
    }

    #[test]
    fn test_budgeted_preserves_message_order() {
        let mut conv = Conversation::new();
        conv.add_user_message("first");
        conv.messages
            .push(Message::new(Role::Assistant, "response 1"));
        conv.add_user_message("second");
        conv.messages
            .push(Message::new(Role::Assistant, "response 2"));
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
                    reasoning_content: None,
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

    /// Regression for DeepSeek `insufficient tool messages following
    /// tool_calls message` 400. An assistant emitted N=3 tool_calls but
    /// only 2 ToolResults arrived before a User text message — the third
    /// call_id never gets a tool message, and strict providers reject.
    /// Sanitize must drop the offending ATC + its partial results so the
    /// surviving prefix preserves the wire-level invariant.
    #[test]
    fn test_sanitize_drops_under_paired_atc_in_middle_of_history() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::System, "sys"),
            Message::new(Role::User, "first"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c3".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    reasoning_content: None,
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok1".into(),
                    success: true,
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c2".into(),
                    output: "ok2".into(),
                    success: true,
                }),
            },
            // c3 result MISSING — the source of the 400.
            Message::new(Role::User, "second"),
        ];
        sanitize_messages(&mut msgs);
        // ATC + 2 partial results gone; surviving = sys + user1 + user2.
        assert_eq!(msgs.len(), 3, "got: {:?}", msgs);
        assert!(matches!(msgs[0].role, Role::System));
        assert_eq!(msgs[1].text(), Some("first"));
        assert_eq!(msgs[2].text(), Some("second"));
    }

    /// Same situation as above, but the boundary is a *next* ATC instead
    /// of a Text message. The first (under-paired) ATC and its partial
    /// results must be dropped; the second (well-paired) ATC stays.
    #[test]
    fn test_sanitize_drops_under_paired_atc_when_followed_by_another_atc() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "a1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "a2".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    reasoning_content: None,
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "a1".into(),
                    output: "ok".into(),
                    success: true,
                }),
            },
            // a2 missing.
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "b1".into(),
                        name: "bash".into(),
                        arguments: "{}".into(),
                    }],
                    reasoning_content: None,
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "b1".into(),
                    output: "ok".into(),
                    success: true,
                }),
            },
        ];
        sanitize_messages(&mut msgs);
        // First ATC + a1 result removed; second ATC + b1 result kept.
        assert_eq!(msgs.len(), 3, "got: {:?}", msgs);
        assert_eq!(msgs[0].text(), Some("go"));
        assert!(matches!(
            msgs[1].content,
            MessageContent::AssistantWithToolCalls { .. }
        ));
        assert!(matches!(msgs[2].content, MessageContent::ToolResult(_)));
    }

    /// Trailing under-paired ATC (no Text / next ATC after it) is the
    /// case the original sanitize already handled. Pinning it here so
    /// the new mid-history logic doesn't accidentally regress the tail
    /// path.
    #[test]
    fn test_sanitize_drops_under_paired_atc_at_tail() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::User, "go"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    reasoning_content: None,
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok".into(),
                    success: true,
                }),
            },
            // c2 missing, conversation ends here.
        ];
        sanitize_messages(&mut msgs);
        // ATC + 1 partial result both removed; just the user message remains.
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), Some("go"));
    }

    /// Negative control: when every ATC's tool_calls are fully paired,
    /// nothing must be removed even though the new mid-history logic
    /// runs over Text boundaries. Catches "fix that throws away valid
    /// history" regressions.
    #[test]
    fn test_sanitize_preserves_fully_paired_history_through_text_boundaries() {
        use crate::tool::{ToolCall, ToolResult};
        let mut msgs = vec![
            Message::new(Role::User, "first"),
            Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    ],
                    reasoning_content: None,
                },
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok1".into(),
                    success: true,
                }),
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c2".into(),
                    output: "ok2".into(),
                    success: true,
                }),
            },
            Message::new(Role::Assistant, "done"),
            Message::new(Role::User, "second"),
        ];
        let len_before = msgs.len();
        sanitize_messages(&mut msgs);
        assert_eq!(msgs.len(), len_before, "must not drop fully-paired history");
    }

    /// Regression: `microcompact` gate tied to `threshold_chars`.
    ///
    /// Before: hardcoded `total_chars < 100_000` meant any ctx with a
    /// real budget under ~25K tokens (Ollama at 8K) could never hit
    /// the gate — per-model `tool_output_cap` optimization was silently
    /// neutralized. Now the threshold is passed in; small-window ctx
    /// passes a proportionally smaller value.
    #[test]
    fn microcompact_respects_threshold_parameter() {
        use crate::tool::{ToolCall, ToolResult};

        // Build 25 turns each with a 1000-char bash result. Total
        // tool-result bytes ≈ 25_000 — well below the old 100K gate
        // but above a 10K gate.
        fn build_msgs() -> Vec<Message> {
            let mut msgs = vec![Message::new(Role::System, "sys")];
            for i in 0..25 {
                msgs.push(Message::new(Role::User, format!("task {}", i)));
                msgs.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::AssistantWithToolCalls {
                        text: None,
                        tool_calls: vec![ToolCall {
                            id: format!("c{}", i),
                            name: "bash".to_string(),
                            arguments: "{}".to_string(),
                        }],
                        reasoning_content: None,
                    },
                });
                msgs.push(Message {
                    role: Role::Tool,
                    content: MessageContent::ToolResult(ToolResult {
                        call_id: format!("c{}", i),
                        output: "x".repeat(1000),
                        success: true,
                    }),
                });
            }
            msgs
        }

        fn total_tool_bytes(msgs: &[Message]) -> usize {
            msgs.iter()
                .map(|m| match &m.content {
                    MessageContent::ToolResult(r) => r.output.len(),
                    _ => 0,
                })
                .sum()
        }

        // High threshold (100K) → total 25K < 100K → no-op.
        let mut msgs_high = build_msgs();
        let before_high_len = msgs_high.len();
        let before_high_bytes = total_tool_bytes(&msgs_high);
        let msg_count_high = msgs_high.len();
        microcompact(&mut msgs_high, msg_count_high, 100_000);
        assert_eq!(
            msgs_high.len(),
            before_high_len,
            "high-threshold run must not drop msgs"
        );
        assert_eq!(
            total_tool_bytes(&msgs_high),
            before_high_bytes,
            "high threshold (25K < 100K) must leave tool_result bytes untouched"
        );

        // Low threshold (10K) → total 25K >= 10K → microcompact kicks
        // in and shrinks older ToolResults.
        let mut msgs_low = build_msgs();
        let before_low_bytes = total_tool_bytes(&msgs_low);
        let msg_count_low = msgs_low.len();
        microcompact(&mut msgs_low, msg_count_low, 10_000);
        let after_low_bytes = total_tool_bytes(&msgs_low);
        assert!(
            after_low_bytes < before_low_bytes,
            "low threshold (25K > 10K) must shrink tool_result bytes, before={} after={}",
            before_low_bytes,
            after_low_bytes
        );
    }

    /// Regression: `build_compression_content` must not cut between an
    /// `AssistantWithToolCalls` and its trailing `ToolResult`(s). Cutting
    /// mid-pair leaves orphan tool_results which `clean_message_pipeline`
    /// silently drops — the model loses edit confirmations. Anthropic API
    /// also rejects orphan tool_results.
    ///
    /// Construct a conversation where the naive cut index
    /// (`len - KEEP_MESSAGES`) lands on a ToolResult whose paired ATC
    /// sits in the drop range. Verify the returned cut index skips past
    /// ALL trailing ToolResults so no orphan survives.
    #[test]
    fn compression_cut_never_splits_tool_use_result_pair() {
        use crate::tool::{ToolCall, ToolResult};

        // Helper: build a conv where messages[cut_idx] = ToolResult
        // with its ATC at messages[cut_idx - 1] (in drop range).
        let build_conv = || {
            let mut conv = Conversation::new();

            // Pad with plain text turns until we reach the position where
            // the problematic tool pair will land.
            // KEEP_MESSAGES = 20. We want naive_cut = len - 20 to hit a
            // ToolResult. If we put ATC at msg[N-21] and ToolResult at
            // msg[N-20], then `conv.len() = N`, `naive_cut = N-20` →
            // lands on the ToolResult. ✓
            //
            // Put a text-only prefix of 20 messages, then ATC+ToolResult,
            // then another 20 text-only suffix → len = 42, naive_cut = 22
            // which SHOULD be the ToolResult we planted.

            for i in 0..10 {
                conv.add_user_message(&format!("prefix task {}", i));
                conv.push_delta(&format!("prefix reply {}", i));
                conv.finalize_stream();
            }
            // After 10 text turns: 20 messages.

            // Position 20 would be the next user msg. But we want ATC here
            // (msg[20]) and ToolResult at msg[21]. Problem: ATC must be
            // preceded by a User in a normal turn. Use a real tool round.
            conv.add_user_message("trigger tool"); // msg[20]
            conv.add_assistant_tool_calls(
                Some("r"),
                vec![ToolCall {
                    // msg[21]
                    id: "call_would_orphan".to_string(),
                    name: "bash".to_string(),
                    arguments: "{}".to_string(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                // msg[22]
                call_id: "call_would_orphan".to_string(),
                output: "tool output that must not be lost".to_string(),
                success: true,
            });
            // After the tool round: 23 messages.

            // Suffix: pad with text turns so len - KEEP_MESSAGES = 22.
            // Need len = 42. Currently 23. Add 19 more → 42.
            // Adding in user/assistant pairs: 19/2 = 9 full + 1 extra.
            for i in 0..9 {
                conv.add_user_message(&format!("suffix task {}", i));
                conv.push_delta(&format!("suffix reply {}", i));
                conv.finalize_stream();
            }
            // 23 + 18 = 41. Add one more user message.
            conv.add_user_message("final task");
            conv
        };

        let conv = build_conv();
        let len = conv.messages.len();
        assert_eq!(len, 42, "conv layout wrong");

        let naive_cut = len - KEEP_MESSAGES;
        assert_eq!(naive_cut, 22);
        // Confirm msg[22] is indeed the ToolResult we planted.
        assert!(
            matches!(conv.messages[22].content, MessageContent::ToolResult(_)),
            "test layout broken: msg[22] should be ToolResult"
        );

        // Now query the real fn. Fix guarantees the cut index points at
        // a position that is NOT a ToolResult (advanced past trailing
        // ToolResults so no orphan survives).
        let (_summary, actual_cut) = build_compression_content(&conv);

        if actual_cut < conv.messages.len() {
            let first_survivor = &conv.messages[actual_cut];
            let is_tool_result = matches!(
                first_survivor.content,
                MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_)
            );
            assert!(
                !is_tool_result,
                "cut index {} lands on ToolResult (naive was {}); \
                 surviving range would start with orphan",
                actual_cut, naive_cut
            );
        }

        // Applied-cut invariant: after draining [..actual_cut], every
        // surviving ToolResult has its paired ATC in the surviving range.
        let mut c2 = build_conv();
        c2.apply_compression(actual_cut, "summary".to_string());

        let mut live_call_ids = std::collections::HashSet::<String>::new();
        for msg in &c2.messages {
            match &msg.content {
                MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                    for tc in tool_calls {
                        live_call_ids.insert(tc.id.clone());
                    }
                }
                MessageContent::ToolResult(r) => {
                    assert!(
                        live_call_ids.contains(&r.call_id),
                        "orphan ToolResult({}) in surviving range — its ATC was dropped",
                        r.call_id
                    );
                }
                _ => {}
            }
        }
    }

    /// Conversation compression is correct only when it reduces the next
    /// wire payload. A generated summary (plus any post-compress state note)
    /// can be larger than the messages it replaces, so callers must judge
    /// compression by before/after `build_messages` tokens, not by raw
    /// history length.
    #[test]
    fn compression_must_be_judged_by_wire_token_savings() {
        let mut conv = Conversation::new();
        for i in 0..16 {
            conv.add_user_message(&format!("task {}", i));
            conv.push_delta("ok");
            conv.finalize_stream();
        }

        let before_tokens: usize = build_messages(&conv, "sys", 64_000, "")
            .0
            .iter()
            .map(|m| m.estimate_tokens())
            .sum();
        let (_mechanical_summary, remove_count) = build_compression_content(&conv);
        assert!(remove_count > 0, "test conversation should be compressible");

        conv.apply_compression(remove_count, "expanded summary ".repeat(2_000));
        conv.add_user_message(
            "[Context was compressed. Here is your current state:]\n\
             TASK: continue the current issue analysis\n\
             RECENTLY READ: crates/atomcode-core/src/agent/mod.rs",
        );

        let after_tokens: usize = build_messages(&conv, "sys", 64_000, "")
            .0
            .iter()
            .map(|m| m.estimate_tokens())
            .sum();

        assert!(
            after_tokens > before_tokens,
            "dropped messages alone is not a valid compaction success metric: \
             before={before_tokens}, after={after_tokens}"
        );
    }

}
