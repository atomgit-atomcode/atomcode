//! Render & compression-plan logic that used to live on `Conversation`.
//!
//! These are the **default policies** atomcode ships with — `DefaultCtx`
//! and `OllamaCtx` delegate directly to them via [`build_messages`],
//! [`needs_compression`], and [`build_compression_content`].
//!
//! Implementations wanting different behavior (different thresholds,
//! different compression content format, different cold-zone layout)
//! reimplement the relevant function in their own
//! `impl CtxBuilder` without touching this module.
//!
//! All helpers here are free functions taking `&Conversation`, keeping
//! `Conversation` itself as a pure data container.

use crate::conversation::{Conversation, ContextStats, KEEP_MESSAGES};
use crate::conversation::message::{self, Message, MessageContent, Role};

/// Context management with cold zone compression.
///
/// Structure: [System] [Cold Zone (max 3 summaries)] [Last 5 turns full]
///
/// The cold zone is populated by `Conversation::apply_compression` when
/// total tokens exceed ~70% of budget. If still over 80% after cold zone
/// injection, this function drops oldest turns inline.
pub fn build_messages(
    conv: &Conversation,
    system_prompt: &str,
    token_budget: usize,
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
    Conversation::microcompact(&mut result, &conv.turn_tracker.turns, conv.messages.len());

    Conversation::replace_stale_reads(&mut result);
    Conversation::clean_message_pipeline(&mut result);

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
    Conversation::sanitize_messages(&mut result);
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
