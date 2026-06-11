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
            "[Earlier conversation history ({} compression{}) — \
             this is a compressed summary of OLDER conversation context, \
             NOT a user instruction]\n{}",
            conv.cold_summaries.len(),
            if conv.cold_summaries.len() > 1 {
                "s"
            } else {
                ""
            },
            conv.cold_summaries.join("\n---\n")
        );
        // Role::User (NOT System): a System-role cold summary gets folded into
        // messages[0] by the consecutive-system merger below, so every new
        // compression rewrote the ~16K system prefix and zeroed the whole cache.
        // As a User message it rides next to the surviving history instead, and
        // the frozen system prompt stays byte-stable across compactions. See
        // `test_no_consecutive_system_messages_after_compression`.
        result.push(Message::new(Role::User, cold_text));
    }

    // Add all current messages
    result.extend(conv.messages.iter().cloned());

    // NOTE: read_file result condensation was here (83fc7ff) but reverted.
    // 问题: 长距离重读是合理需求（旧内容被压缩后模型需要重新看），
    // 短距离重读在 keep_recent 保护内又压缩不到。两头不讨好。
    // 正确方案需要更深入设计，不在这里做。

    // Safety: drop oldest turns — but ONLY as a true last-resort, at the same
    // point the persisted compaction targets (`auto_compact_threshold`,
    // budget − 13K ≈ 98.7% on 1M). Skip if cold_summaries exist — that means
    // LLM compression just ran and we're looking at the "keep_full=5" survivor
    // set; dropping those too would wipe ALL context (the sent=0 audit bug).
    //
    // This trigger used to be budget×80% — BELOW the persisted compaction
    // threshold. A long session then lived in the 80%–98.7% band managed
    // SOLELY by this path, which is recomputed from the (still-growing)
    // conversation on every render: each time one more old turn had to be
    // folded, the `[Context overflow: N earlier turns compressed]` digest
    // changed (N→N+1) and the survivor cut slid forward, so the provider
    // prefix cache broke every few turns. Measured on deepseek-v4-flash
    // (2026-06-04): this was 50.8% of v4.25.0's remaining cache misses
    // (cached 0.02元/M vs uncached 1元/M, 50×). Triggering at
    // `auto_compact_threshold` lets the persisted compaction (task-boundary
    // in agent/mod.rs + mid-turn `maybe_compress_history`) own context
    // management — it breaks the prefix ONCE then freezes — and keeps this
    // render append-only across the whole band. See
    // `render_drop_defers_to_persisted_compaction_in_upper_band`.
    //
    // When it DOES fire (persisted compaction didn't run, or a single
    // oversized turn blows the budget in one shot), drop down to 80% so there
    // is a real buffer before it could fire again, rather than sitting on the
    // trigger line and re-dropping every turn.
    let drop_trigger = auto_compact_threshold(token_budget);
    let drop_target = token_budget * 80 / 100;
    let total_tokens: usize = result.iter().map(|m| m.estimate_tokens()).sum();
    let mut dropped_tokens = 0usize;

    if total_tokens > drop_trigger && conv.cold_summaries.is_empty() {
        let tokens_to_drop = total_tokens - drop_target;

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
            // Role::User, same rationale as the cold-zone summary above: keep
            // the compaction digest out of messages[0] so the frozen system
            // prompt survives compaction in the prefix cache.
            result.push(Message::new(Role::User, digest));
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

    // Prior-turn ToolResult stubbing now happens via the COMMITTED
    // `collapse_committed` (called on `conv.messages` before render, in
    // turn/runner.rs) — NOT here. build_messages stays a pure renderer so
    // the rendered prefix is byte-stable across turns. The 80% FINAL BYTE
    // CEILING below remains the render-time overflow backstop.

    // NOTE (prompt-cache): we intentionally do NOT refresh historical
    // `read_file` results from current disk here. An earlier version called
    // `replace_stale_reads(&mut result)` so the model would "always see the
    // latest code" after an edit — but that re-rendered an OLD tool_result
    // message (same tool_call_id) from the file's *current* bytes. Once any
    // file was edited, every prior read of it changed (shifted line-number
    // gutters, re-formatted fences) → the sent history mutated mid-stream →
    // the provider prefix cache collapsed from the first rewritten message
    // onward. On deepseek-v4-flash that is a 50× cost cliff (cached
    // 0.02元/M vs uncached 1元/M); measured hit-rate dropped from ~99% to
    // 4–20% on long edit-heavy sessions. History is now append-only and
    // byte-stable: a tool_result is frozen as first rendered. The model still
    // sees the latest content whenever it needs it by simply reading again in
    // the current turn — that appends at the tail and grows the cache prefix
    // instead of breaking it. See `historical_read_results_are_frozen_after_edit`.
    //
    // sanitize_messages drops AssistantWithToolCalls whose tool_calls
    // didn't all get followed by matching tool_result messages before
    // a non-tool boundary (next ATC / Text / MultiPart). Required to
    // satisfy DeepSeek's strict `insufficient tool messages following
    // tool_calls message` 400 and the equivalent Claude/OpenAI/Gemini
    // pairing contracts. Several upstream paths can leave the
    // conversation in this state (cancel mid-batch, hard-truncate
    // landing between ATC and its results, /resume of an old session)
    // — sanitizing at send time is the defensive backstop that catches
    // them all uniformly. Already wired into the fallback path
    // (`build_messages_fallback`); this call extends the same safety net
    // to the main turn-tracked path. Runs BEFORE clean_message_pipeline
    // so the consecutive-User merger downstream can collapse any
    // adjacent User messages that the dropped ATC was previously
    // separating.
    sanitize_messages(&mut result);
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
    // collapse_committed (called before render in turn/runner.rs) handles
    // prior-turn stubbing; the 80% drop path above skips entirely when
    // `cold_summaries` is populated (legacy protection against a
    // since-fixed pathology). That leaves the recent window with no byte
    // enforcement, so accumulated mid-sized ToolResults can still blow the
    // budget. Single oldest-first forward pass: condense each ToolResult
    // once only (idempotent `condensed()` would otherwise spin), stop as
    // soon as the total fits under 80% of budget. The last 4 messages
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
pub fn build_compression_content(conv: &Conversation, keep_ceiling: usize) -> (String, usize) {
    // Count-based starting cut: compress everything older than the last
    // KEEP_MESSAGES. `saturating_sub` (not an early `len <= KEEP_MESSAGES`
    // bail) because that old guard was a pure *message-count* gate — it let
    // reasoning-heavy thinking sessions sit at <20 messages yet >>window
    // tokens (the deepseek-v4 overflow). The token-aware loop below now
    // drives the decision; we bail at the very end only if nothing needs
    // dropping (compress_end_idx still 0 after the loop).
    let mut compress_end_idx = conv.messages.len().saturating_sub(KEEP_MESSAGES);

    // ── Token-aware keep-floor ──
    // KEEP_MESSAGES is a *message-count* floor; on reasoning-heavy thinking
    // models (deepseek-v4 etc.) the kept tail's `reasoning_content` — which
    // the API requires echoed back in full and which NO reducer condenses —
    // can alone exceed the window. So advance the cut FORWARD (drop more old
    // rounds) until the kept tail fits under `keep_ceiling` tokens. We drop
    // whole rounds (the only API-legal way to shed echoed reasoning), never
    // strip reasoning from a kept message. Floor: always keep the final round
    // so the model still sees its latest action.
    let last_round_start = conv
        .messages
        .iter()
        .rposition(|m| matches!(m.role, Role::User | Role::Assistant))
        .unwrap_or(0);
    while compress_end_idx < last_round_start {
        let tail_tokens: usize = conv.messages[compress_end_idx..]
            .iter()
            .map(|m| m.estimate_tokens())
            .sum();
        if tail_tokens <= keep_ceiling {
            break;
        }
        // Advance to the next round boundary (next User/Assistant), bounded
        // so the final round always survives.
        compress_end_idx = (compress_end_idx + 1..=last_round_start)
            .find(|&i| matches!(conv.messages[i].role, Role::User | Role::Assistant))
            .unwrap_or(last_round_start);
    }

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

/// Build the generic compaction stub used by both `collapse_committed`
/// (normal-path committed collapse) and the emergency
/// `compact_old_tool_results_in_place`. Tool name comes from the model's
/// own tool_calls so the framework adds zero hardcoded tool knowledge —
/// every tool gets the same shape.
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
/// Same category as the `read_file` exemption in `collapse_committed`.
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
/// `collapse_committed`. This is the destructive emergency counterpart:
/// the former ephemeral `microcompact` (render-time, non-committed) no
/// longer exists — `collapse_committed` is the normal committed path
/// called before each render. This function runs from the agent emergency
/// path and permanently shrinks `conv.messages` so the next
/// `needs_compression` check sees the freed budget.
///
/// Idempotent: stubs already in place are smaller than MIN_COLLAPSE_SIZE
/// and skip the rewrite.
pub(crate) fn compact_old_tool_results_in_place(
    conv: &mut crate::conversation::Conversation,
    keep_recent_turns: usize,
    exempt_read_file: bool,
) {
    let turns = &conv.turn_tracker.turns;
    if turns.len() <= keep_recent_turns {
        return;
    }
    let cutoff_turn = turns.len() - keep_recent_turns;
    let cutoff_msg = if cutoff_turn < turns.len() {
        turns[cutoff_turn].start_idx.min(conv.messages.len())
    } else {
        // keep_recent_turns == 0: no turn is "recent", stub everything.
        conv.messages.len()
    };

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
        if exempt_read_file && tool_name == "read_file" {
            continue;
        }
        let summary = build_compact_stub(tool_name, &tr.output, tr.success);
        conv.messages[i].content = MessageContent::ToolResult(crate::tool::ToolResult {
            call_id: tr.call_id.clone(),
            output: summary,
            success: tr.success,
        });
    }
}

/// Normal-path committed collapse (cache-friendly replacement for the
/// removed ephemeral `microcompact`). Threshold-gated by the same
/// `70% × budget × 4` char trigger; below it this is a no-op so short
/// sessions stay full-fidelity and byte-stable (append-only). Above it,
/// permanently stubs old (non-active-turn, non-`read_file`) ToolResults in
/// `conv.messages`. Idempotent + monotonic via `compact_old_tool_results_in_place`
/// (`keep_recent_turns = 1` → keeps everything after the last `Role::User`,
/// i.e. the active turn). Because it commits, the stubbed prefix never
/// changes again across turns — the property `microcompact` violated.
pub(crate) fn collapse_committed(conv: &mut crate::conversation::Conversation, token_budget: usize) {
    let threshold_chars = (token_budget as u64 * 4 * 70 / 100) as usize;
    let total_chars: usize = conv
        .messages
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
    compact_old_tool_results_in_place(conv, /* keep_recent_turns */ 1, /* exempt_read_file */ true);
}

// `replace_stale_reads` was removed: it mutated historical `read_file`
// tool_results in place from current disk bytes whenever their file had been
// edited, which broke the provider prompt-prefix cache mid-conversation (see
// the NOTE about `replace_stale_reads` in `build_messages`, and the
// `historical_read_results_are_frozen_after_edit` regression test). History is
// kept append-only and byte-stable; the model re-reads in the current turn when
// it needs the latest content.

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
    fn large_window_ignores_message_count_for_compaction() {
        // Regression guard for the task-boundary compaction fix
        // (agent/mod.rs handle path). Task-boundary compaction is now gated
        // SOLELY on this budget check — the old `messages.len() >
        // KEEP_MESSAGES` trigger was removed. A long conversation (well past
        // KEEP_MESSAGES=20) that is far under a large (1M) window must NOT
        // need compression: otherwise every follow-up message rewrites the
        // prompt prefix and collapses provider prompt-prefix cache (~10% hit
        // on the first request after each follow-up — observed on the 1M
        // setup). The SAME conversation on a 128K window does need it.
        let mut conv = Conversation::new();
        // 30 messages (>20), ~150K tokens total (≈ bytes / 4).
        for _ in 0..15 {
            conv.messages
                .push(Message::new(Role::User, "x".repeat(20_000)));
            conv.messages
                .push(Message::new(Role::Assistant, "y".repeat(20_000)));
        }
        let total: usize = conv.messages.iter().map(|m| m.estimate_tokens()).sum();
        assert_eq!(conv.messages.len(), 30, "fixture exceeds KEEP_MESSAGES=20");
        assert!(
            (120_000..400_000).contains(&total),
            "fixture should be ~150K tokens, got {}",
            total
        );

        // 1M window: ~150K is ~15% of budget — ample room, must NOT compress
        // despite 30 messages. This is the bug the fix addresses.
        assert!(
            !needs_compression(&conv, 0, 1_000_000),
            "large window must not compress a far-under-budget conversation regardless of message count"
        );
        // 128K window: ~150K exceeds the ~115K threshold — must still compress.
        assert!(
            needs_compression(&conv, 0, 128_000),
            "small window over threshold must still compress"
        );
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
                    synthetic: false,
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

    /// FAITHFUL REPRO + regression guard: same single-turn deepseek-v4 scenario,
    /// but now with
    /// the real auto-compression cycle (`maybe_compress_history` in
    /// agent/mod.rs) simulated after every round — Tier1
    /// (`compact_old_tool_results_in_place`, keep 3) then Tier2
    /// (`build_compression_content` + `apply_compression`). This answers the
    /// question the pure-render repro above cannot: does auto-compaction
    /// keep the payload under the window?
    ///
    /// Hypothesis under test: NO. Compaction drains OLD rounds (reasoning and
    /// all) into the cold zone, but the keep-floor (last KEEP_MESSAGES=20
    /// messages ≈ 10 rounds) is preserved at full fidelity — and 10 rounds of
    /// heavy reasoning_content alone exceed the 128K window. If this asserts
    /// green, the bug is render-only and compaction already protects prod; if
    /// red, compaction genuinely cannot bound reasoning-heavy single turns.
    #[test]
    fn repro_reasoning_overflow_with_auto_compaction() {
        use crate::tool::{ToolCall, ToolResult};

        // Mirror maybe_compress_history: single pass, Tier1 then Tier2.
        fn simulate_compress(conv: &mut Conversation, sys_tokens: usize, budget: usize) {
            if !needs_compression(conv, sys_tokens, budget) {
                return;
            }
            compact_old_tool_results_in_place(conv, 3, false);
            if !needs_compression(conv, sys_tokens, budget) {
                return;
            }
            // Mirror Agent::compaction_keep_ceiling (no tool defs in this test):
            // window − system − cold-zone − output reserve.
            let cold_zone: usize = conv.cold_summaries.iter().map(|s| s.len() / 4 + 4).sum();
            let output_reserve = (budget / 4).clamp(8_000, 16_384);
            let keep_ceiling = budget
                .saturating_sub(sys_tokens)
                .saturating_sub(cold_zone)
                .saturating_sub(output_reserve)
                .max(budget / 4);
            let (content, n) = build_compression_content(conv, keep_ceiling);
            if !content.is_empty() && n > 0 {
                conv.apply_compression(n, content);
            }
        }

        let window = 128_000usize;
        let sys = "sys";
        let sys_tokens = sys.len() / 4 + 4;

        let mut conv = Conversation::new();
        conv.add_user_message("read the whole repo and explain the architecture");

        for round in 0..15 {
            let call = ToolCall {
                id: format!("call_{}", round),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/src/f_{}.rs"}}"#, round),
            };
            conv.add_assistant_tool_calls(
                Some("looking..."),
                vec![call],
                Some(&"思考过程".repeat(10_000)),
            );
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", round),
                output: "x".repeat(8_000),
                success: true,
            });
            // Auto-compression runs at the TOP of each turn in the real loop.
            simulate_compress(&mut conv, sys_tokens, window);
        }

        let (msgs, _stats) = build_messages(&conv, sys, window, "");
        let total: usize = msgs.iter().map(|m| m.estimate_tokens()).sum();

        assert!(
            total <= window,
            "even WITH auto-compaction the payload is {total} tok > {window} \
             window — the KEEP_MESSAGES=20 keep-floor of reasoning-heavy rounds \
             can't be compressed (faithful repro of the prod overflow)."
        );
    }

    /// Regression for the 1M-ctx drop-too-early bug: with `token_budget`
    /// = 1_000_000, a ~200K-token conversation should NOT trigger the
    /// drop-oldest-turns path (200K is comfortably under 80% = 800K).
    /// Pre-fix the threshold was `min(budget × 80%, 60K)`, so 200K
    /// > 60K → `dropped_tokens > 0` and the model saw a pruned
    /// transcript despite the ostensibly huge context window.
    ///
    /// Companion to `test_budgeted_drops_oldest_turns_when_over_budget`
    /// (small budget, drop fires) — together they pin both ends of the
    /// scaling: drop must fire at the 80% threshold, not at a fixed
    /// 60K floor.
    #[test]
    fn budgeted_does_not_drop_under_80pct_of_1m_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // Build ~200K tokens of conversation: 25 tool results × 8000
        // tokens each (32000 chars / 4 = 8000). Stays well under 80%
        // of 1M (= 800K), so no drop should fire.
        for turn in 0..25 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("call_{}", turn),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/tmp/f_{}.rs"}}"#, turn),
            };
            conv.add_assistant_tool_calls(None, vec![call], None);
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", turn),
                output: "x".repeat(32_000),
                success: true,
            });
        }
        conv.add_user_message("now what?");

        let (_msgs, stats) = build_messages(&conv, "sys", 1_000_000, "");
        assert_eq!(
            stats.dropped_tokens, 0,
            "200K tokens under 1M budget (80%=800K) must not drop \
             any turns — pre-fix the 60K cap clipped this to 60K and \
             dropped most of the history despite the 1M window."
        );
    }

    /// Companion to the no-drop test above: same 1M budget, but now
    /// the conversation genuinely exceeds 80% (= 800K). Drop MUST
    /// fire, otherwise the safety floor is missing.
    #[test]
    fn budgeted_drops_when_over_80pct_of_1m_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();

        // ~1M tokens of conversation: 32 tool results × ~32K tokens
        // each (128K chars / 4 = 32K). Total well above 800K.
        for turn in 0..32 {
            conv.add_user_message(&format!("task {}", turn));
            let call = ToolCall {
                id: format!("call_{}", turn),
                name: "read_file".to_string(),
                arguments: format!(r#"{{"file_path":"/tmp/f_{}.rs"}}"#, turn),
            };
            conv.add_assistant_tool_calls(None, vec![call], None);
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", turn),
                output: "x".repeat(128_000),
                success: true,
            });
        }
        conv.add_user_message("now what?");

        let (msgs, stats) = build_messages(&conv, "sys", 1_000_000, "");
        assert!(
            stats.dropped_tokens > 0,
            "over-800K conversation on 1M budget must trigger the \
             80% drop floor"
        );
        // Latest user message must still survive (HARD FLOOR invariant).
        assert_eq!(msgs.last().unwrap().text(), Some("now what?"));
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
    fn collapse_committed_skips_read_file_to_preserve_long_session_context() {
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

        // Pad with bash so total_chars crosses collapse_committed's
        // threshold. Use a small budget (40K tokens → 112K char
        // threshold) so the 30 padding bashes + the read_file body
        // (~125K chars total) reliably triggers collapse_committed.
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

        // collapse_committed now handles prior-turn stubbing; call it
        // as turn/runner.rs does, before the render.
        collapse_committed(&mut conv, 40_000);
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
        // because collapse_committed was a no-op.
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
        // Messages reduced, but the first real User message (task 0) is
        // carved out and preserved by apply_compression's sacred-set logic
        // (af3d1ac7), so it survives as its own turn alongside tasks 3-7:
        // task0 + task3,4,5,6,7 = 6 turns (not a clean 8 - 3 = 5).
        assert_eq!(conv.turn_tracker.turns.len(), 6);

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

        // The system message keeps ONLY the original prompt — the cold-zone
        // summary must NOT be merged into it (that rewrote messages[0] on every
        // compression and zeroed the prefix cache). The summary now rides on a
        // User message so the model still retains the context.
        let sys = msgs
            .iter()
            .find(|m| matches!(m.role, Role::System))
            .and_then(|m| m.text())
            .expect("at least one system message");
        assert!(
            sys.contains("you are atomcode"),
            "system must keep original prompt"
        );
        assert!(
            !sys.contains("Earlier conversation history"),
            "cold-zone summary must NOT be in the system message (prefix-cache stability)"
        );
        let summary_in_user = msgs.iter().any(|m| {
            matches!(m.role, Role::User)
                && m.text().map_or(false, |t| t.contains("Earlier conversation history"))
        });
        assert!(
            summary_in_user,
            "cold-zone summary must ride on a User message so context is retained"
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

        // 20 turns, each with a 6K-char bash result. `collapse_committed`
        // only runs before rendering (called from turn/runner.rs) so it
        // does not fire here; the recent window (≈ last 6-7 turns / 20
        // messages) sums to > 36K chars ≈ 9K+ est tokens, which exceeds
        // the 80% FINAL BYTE CEILING backstop exercised by this test.
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
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".to_string(),
                    output: "ok".to_string(),
                    success: true,
                }),
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok1".into(),
                    success: true,
                }),
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c2".into(),
                    output: "ok2".into(),
                    success: true,
                }),
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "a1".into(),
                    output: "ok".into(),
                    success: true,
                }),
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "b1".into(),
                    output: "ok".into(),
                    success: true,
                }),
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok".into(),
                    success: true,
                }),
                            synthetic: false,
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
                    thinking_blocks: Vec::new(),
                },
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c1".into(),
                    output: "ok1".into(),
                    success: true,
                }),
                            synthetic: false,
            },
            Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: "c2".into(),
                    output: "ok2".into(),
                    success: true,
                }),
                            synthetic: false,
            },
            Message::new(Role::Assistant, "done"),
            Message::new(Role::User, "second"),
        ];
        let len_before = msgs.len();
        sanitize_messages(&mut msgs);
        assert_eq!(msgs.len(), len_before, "must not drop fully-paired history");
    }

    /// End-to-end regression for the DeepSeek `insufficient tool
    /// messages following tool_calls message` 400 via the main
    /// turn-tracked `build_messages` path. The function-level
    /// `sanitize_messages` tests cover the unit; this test pins the
    /// wiring — sanitize_messages must run from `build_messages`, not
    /// just from the fallback. Constructs a Conversation with a
    /// turn-bearing under-paired ATC mid-history (ATC(3) + only 2
    /// tool_results, then a fresh user turn) and verifies the wire-
    /// level invariant holds in the output: every surviving ATC is
    /// followed by exactly N tool messages.
    #[test]
    fn build_messages_satisfies_atc_pairing_after_under_paired_mid_history() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("first task");
        conv.add_assistant_tool_calls(
            None,
            vec![
                ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() },
                ToolCall { id: "c2".into(), name: "bash".into(), arguments: "{}".into() },
                ToolCall { id: "c3".into(), name: "bash".into(), arguments: "{}".into() },
            ],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "c1".into(),
            output: "ok1".into(),
            success: true,
        });
        conv.add_tool_result(ToolResult {
            call_id: "c2".into(),
            output: "ok2".into(),
            success: true,
        });
        // c3's ToolResult never lands — repro for DeepSeek 400.
        conv.add_user_message("second task");

        let (msgs, _stats) = build_messages(&conv, "sys", 8000, "");

        // Walk the result and assert every ATC is followed by exactly
        // N consecutive tool-role messages — the wire invariant
        // OpenAI / DeepSeek / Claude / Gemini all require.
        let mut i = 0;
        while i < msgs.len() {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msgs[i].content {
                let n = tool_calls.len();
                for j in 0..n {
                    let next_idx = i + 1 + j;
                    assert!(
                        next_idx < msgs.len(),
                        "ATC at {} expects {} tool_results but messages end at {}: {:?}",
                        i,
                        n,
                        msgs.len(),
                        msgs.iter().map(|m| &m.role).collect::<Vec<_>>()
                    );
                    assert!(
                        matches!(
                            msgs[next_idx].content,
                            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_)
                        ),
                        "ATC at {} expects tool_result at {} but found {:?}",
                        i,
                        next_idx,
                        msgs[next_idx].role
                    );
                }
                i += 1 + n;
            } else {
                i += 1;
            }
        }

        // Defensive: the orphan c3 must NOT appear as a tool_call_id
        // anywhere in the output (the under-paired ATC was dropped, so
        // c1 and c2 are gone with it).
        for m in &msgs {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                for tc in tool_calls {
                    assert_ne!(tc.id, "c3", "dropped ATC's call_ids must not survive");
                    assert_ne!(tc.id, "c1");
                    assert_ne!(tc.id, "c2");
                }
            }
            if let MessageContent::ToolResult(r) = &m.content {
                assert_ne!(r.call_id, "c1", "partial tool_results must not survive");
                assert_ne!(r.call_id, "c2");
            }
        }
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
        let (_summary, actual_cut) = build_compression_content(&conv, usize::MAX);

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
        let (_mechanical_summary, remove_count) = build_compression_content(&conv, usize::MAX);
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

    /// Regression: a historical `read_file` tool result must stay byte-for-byte
    /// frozen as it was first rendered, even after the same file is edited later
    /// in the session. Re-rendering it from current disk state (shifted line
    /// numbers / different content) mutates a message in the MIDDLE of the sent
    /// history, which collapses the provider prefix cache from that point on
    /// (deepseek-v4-flash: cached 0.02元/M vs uncached 1元/M — a 50× cliff).
    /// The model can always see the latest content by reading again in the
    /// current turn; it must never come from rewriting old messages.
    #[test]
    fn historical_read_results_are_frozen_after_edit() {
        use crate::tool::{ToolCall, ToolResult};

        // A real file on disk whose CURRENT content differs from what we stored
        // in history when it was first read.
        let path = std::env::temp_dir().join(format!(
            "atomcode_frozen_read_test_{}.rs",
            std::process::id()
        ));
        std::fs::write(&path, "fn main() {}\n// EDITED LINE A\n// EDITED LINE B\n").unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mut conv = Conversation::new();
        conv.add_user_message("read the file");

        // Turn 1: read_file → frozen result (single line, old gutter).
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_read".into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"file_path":"{path_str}"}}"#),
            }],
            None,
        );
        let frozen_read = "   1| fn main() {}".to_string();
        conv.add_tool_result(ToolResult {
            call_id: "call_read".into(),
            output: frozen_read.clone(),
            success: true,
        });

        // Turn 2: edit_file on the SAME path → marks the file as edited.
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "call_edit".into(),
                name: "edit_file".into(),
                arguments: format!(r#"{{"file_path":"{path_str}"}}"#),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "call_edit".into(),
            output: "Edited successfully".into(),
            success: true,
        });
        conv.add_user_message("continue");

        let (msgs, _stats) = build_messages(&conv, "sys", 1_000_000, "");
        std::fs::remove_file(&path).ok();

        let read_result = msgs.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "call_read" => Some(r.output.clone()),
            _ => None,
        });
        assert_eq!(
            read_result.as_deref(),
            Some(frozen_read.as_str()),
            "historical read_file result was re-rendered from live disk — \
             the sent prefix is no longer byte-stable and the prompt cache breaks"
        );
    }

    /// Acceptance criterion: between two consecutive turns — even when the file
    /// is edited ON DISK in between — turn N's rendered messages must remain a
    /// byte-exact PREFIX of turn N+1's. The cache only grows at the tail; it
    /// never gets cut mid-stream. This is the property the deepseek-v4-flash
    /// prefix cache relies on.
    #[test]
    fn consecutive_turns_keep_byte_stable_prefix_across_disk_edit() {
        use crate::tool::{ToolCall, ToolResult};

        let path = std::env::temp_dir().join(format!(
            "atomcode_prefix_stable_test_{}.rs",
            std::process::id()
        ));
        std::fs::write(&path, "line one\nline two\nline three\n").unwrap();
        let path_str = path.to_string_lossy().to_string();

        // ── Turn N: a read of the file, then a user message. ──
        let mut conv = Conversation::new();
        conv.add_user_message("look at the file");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "c_read".into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"file_path":"{path_str}"}}"#),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "c_read".into(),
            output: "   1| line one\n   2| line two\n   3| line three".into(),
            success: true,
        });
        conv.add_user_message("now edit it");

        let (turn_n, _) = build_messages(&conv, "sys", 1_000_000, "");

        // ── Between turns: the model edits the file (disk content changes,
        //    line numbers shift) and a new user turn begins. ──
        std::fs::write(&path, "INSERTED HEADER\nline one\nline two\nline three\n").unwrap();
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall {
                id: "c_edit".into(),
                name: "edit_file".into(),
                arguments: format!(r#"{{"file_path":"{path_str}"}}"#),
            }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "c_edit".into(),
            output: "Edited successfully".into(),
            success: true,
        });
        conv.add_user_message("what changed?");

        let (turn_n1, _) = build_messages(&conv, "sys", 1_000_000, "");
        std::fs::remove_file(&path).ok();

        assert!(
            turn_n1.len() > turn_n.len(),
            "turn N+1 must extend turn N at the tail"
        );
        // Compare by (role + content) so any mid-stream mutation surfaces as a
        // prefix break, not just a length change.
        let fingerprint = |m: &Message| format!("{:?}|{:?}", m.role, m.content);
        for (i, m) in turn_n.iter().enumerate() {
            assert_eq!(
                fingerprint(&turn_n1[i]),
                fingerprint(m),
                "prefix diverged at message {i}: turn N+1 rewrote a historical \
                 message instead of only appending — prompt cache would break here"
            );
        }
    }

    #[test]
    fn render_drop_defers_to_persisted_compaction_in_upper_band() {
        // Regression: the render-time drop fallback used to trigger at 80% of
        // budget — BELOW the persisted auto-compaction threshold
        // (`auto_compact_threshold` ≈ budget − 13K ≈ 98.7% on 1M). A long
        // session then lived in the 80%–98.7% band managed SOLELY by the
        // render-drop, whose `[Context overflow: N earlier turns compressed]`
        // digest and survivor boundary are recomputed from the (still-growing)
        // conversation on every render. Each time one more old turn had to be
        // folded, the digest changed (N→N+1) and the survivor cut slid forward
        // → the prompt-prefix cache broke every few turns. Measured on
        // deepseek-v4-flash (2026-06-04): 50.8% of 4.25.0's remaining misses.
        //
        // The drop path must now DEFER to the persisted compaction in this
        // band: stay dormant and keep build_messages append-only. It only
        // fires as a true last-resort once total exceeds the same threshold
        // the persisted path uses (verified by the emergency check below).
        let budget = 100_000usize;
        let trigger = auto_compact_threshold(budget); // 95K (5K buffer ≤100K window)
        let eighty = budget * 80 / 100; // 80K — the old (broken) trigger

        // Build N real turns (must go through the API so `turn_tracker` is
        // populated — `build_messages` returns the fallback path on empty
        // turns and never exercises the drop logic). Each turn: a big user
        // message + a short assistant reply, separated so they don't merge.
        let turn = |c: &mut Conversation, i: usize, user_chars: usize| {
            c.add_user_message(&format!("{i} {}", "x".repeat(user_chars)));
            c.push_delta(&format!("ok {i}"));
            c.finalize_stream();
            c.turn_tracker.complete_current();
        };
        // ~84K tokens of history: sits squarely in the [80%, threshold) band.
        let base = || {
            let mut c = Conversation::new();
            for i in 0..42 {
                turn(&mut c, i, 8_000);
            }
            c
        };
        let conv = base();
        let total: usize = conv.messages.iter().map(|m| m.estimate_tokens()).sum();
        assert!(
            total > eighty && total < trigger,
            "precondition: total {total} must sit in [80%={eighty}, threshold={trigger})"
        );

        let (msgs, _) = build_messages(&conv, "sys", budget, "");
        let has_drop_digest = msgs.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("earlier turns compressed"))
        });
        assert!(
            !has_drop_digest,
            "render-drop fired inside the 80%–98.7% band; it must defer to the \
             persisted compaction (which breaks the prefix once then freezes) \
             instead of pre-empting it and sliding the boundary every few turns"
        );

        // Append-only across the next turn: no historical message rewritten.
        let mut conv2 = base();
        turn(&mut conv2, 42, 8_000);
        let (msgs2, _) = build_messages(&conv2, "sys", budget, "");
        let fp = |m: &Message| format!("{:?}|{:?}", m.role, m.content);
        for (i, m) in msgs.iter().enumerate() {
            assert_eq!(
                fp(&msgs2[i]),
                fp(m),
                "prefix diverged at message {i}: render stopped being append-only in the band"
            );
        }

        // Emergency still works: once total clears the threshold, the
        // last-resort drop MUST engage (we never want a true overflow).
        let mut over = base();
        turn(&mut over, 99, 60_000); // +~15K → over 95K
        let (over_msgs, _) = build_messages(&over, "sys", budget, "");
        let fired = over_msgs.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("earlier turns compressed"))
        });
        assert!(
            fired,
            "render-drop must still engage as a last-resort once total exceeds the threshold"
        );
    }

    #[test]
    fn compact_old_exempts_read_file_when_flagged() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "r".into(), name: "read_file".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "r".into(),
            output: format!("L1\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1"); // active turn → kept full

        compact_old_tool_results_in_place(&mut conv, 1, true);

        let get = |cid: &str, conv: &Conversation| {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        assert!(!get("r", &conv).starts_with('['), "read_file must stay full when exempt");
        assert!(get("b", &conv).starts_with("[bash "), "bash must be stubbed");
    }

    #[test]
    fn compact_old_stubs_read_file_when_not_exempt() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "r".into(), name: "read_file".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "r".into(),
            output: format!("L1\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1");

        compact_old_tool_results_in_place(&mut conv, 1, false);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "r" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert!(out.starts_with("[read_file "), "emergency path must still stub read_file");
    }

    #[test]
    fn collapse_committed_noop_below_threshold() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(1_000)),
            success: true,
        });
        conv.add_user_message("t1");

        // budget 1_000_000 → threshold 2.8M chars; ~1K payload → no-op.
        collapse_committed(&mut conv, 1_000_000);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        // The original bash output starts with "[elapsed:..." — a stub would start with "[bash ".
        // Below threshold nothing should be stubbed.
        assert!(!out.starts_with("[bash "), "below threshold must stay full (append-only)");
    }

    #[test]
    fn collapse_committed_stubs_old_keeps_active_and_exempts_read_file() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        // 3 old turns: each a big read_file + big bash; then an active turn.
        for n in 0..3 {
            conv.add_user_message(&format!("t{n}"));
            let r = format!("r{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: r.clone(), name: "read_file".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: r,
                output: format!("L1\n{}", "x".repeat(6_000)),
                success: true,
            });
            let b = format!("b{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: b.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: b,
                output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
                success: true,
            });
        }
        // active turn (kept full): a bash that must NOT be stubbed.
        conv.add_user_message("active");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "ba".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "ba".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
            success: true,
        });

        // budget 8_000 → threshold 22_400 chars; payload ~42K → fires.
        collapse_committed(&mut conv, 8_000);

        let get = |cid: &str, conv: &Conversation| {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        assert!(get("b0", &conv).starts_with("[bash "), "old bash must be stubbed");
        // read_file output starts with "L1\n..." — check it was not replaced with a stub.
        assert!(!get("r0", &conv).starts_with('['), "old read_file must stay full (exempt)");
        // Active-turn bash output starts with "[elapsed:..." — a stub would start with "[bash ".
        assert!(!get("ba", &conv).starts_with("[bash "), "active-turn bash must stay full");
    }

    #[test]
    fn collapse_committed_is_idempotent() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(30_000)),
            success: true,
        });
        conv.add_user_message("t1");

        collapse_committed(&mut conv, 8_000);
        let after_first = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        collapse_committed(&mut conv, 8_000);
        let after_second = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert_eq!(after_first, after_second, "re-running must not re-stub (idempotent)");
    }

    #[test]
    fn compact_old_noop_when_turns_below_keep_recent() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(10_000)),
            success: true,
        });
        // Only 1 turn, keep_recent_turns=1 → early return, no modification.
        let before_count = conv.messages.len();
        compact_old_tool_results_in_place(&mut conv, 1, false);
        assert_eq!(conv.messages.len(), before_count, "must be noop when turns <= keep_recent_turns");
    }

    #[test]
    fn compact_old_skips_output_below_min_collapse_size() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        // Output smaller than MIN_COLLAPSE_SIZE (500) — must not be stubbed.
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: "tiny output".into(),
            success: true,
        });
        conv.add_user_message("t1");

        compact_old_tool_results_in_place(&mut conv, 1, false);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert_eq!(out, "tiny output", "output below MIN_COLLAPSE_SIZE must stay intact");
    }

    #[test]
    fn compact_old_stubs_unknown_tool() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "x".into(), name: "mystery_tool".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "x".into(),
            output: format!("{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1");

        compact_old_tool_results_in_place(&mut conv, 1, true);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "x" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert!(out.starts_with("[mystery_tool "), "unknown tool must still be stubbed with its name");
    }

    #[test]
    fn collapse_committed_noop_on_empty_conversation() {
        let mut conv = Conversation::new();
        // Must not panic on empty conversation.
        collapse_committed(&mut conv, 8_000);
        assert!(conv.messages.is_empty(), "empty conv must stay empty");
    }

    #[test]
    fn collapse_committed_noop_on_zero_budget() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1");

        // Zero budget → threshold = 0 → total_chars > 0 → fires, but stub size
        // (< 500) won't shrink → no-op. This tests the MIN_COLLAPSE_SIZE guard.
        let snapshot_len = conv.messages.len();
        collapse_committed(&mut conv, 0);
        assert_eq!(conv.messages.len(), snapshot_len, "zero budget, but conv unchanged");
    }

    #[test]
    fn compact_old_multiple_consecutive_exempt_read_files() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        // Two consecutive read_file calls in the same turn.
        conv.add_assistant_tool_calls(
            None,
            vec![
                ToolCall { id: "r1".into(), name: "read_file".into(), arguments: "{}".into() },
                ToolCall { id: "r2".into(), name: "read_file".into(), arguments: "{}".into() },
            ],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "r1".into(),
            output: format!("{}", "x".repeat(3_000)),
            success: true,
        });
        conv.add_tool_result(ToolResult {
            call_id: "r2".into(),
            output: format!("{}", "y".repeat(3_000)),
            success: true,
        });
        // Followed by a bash that should be stubbed.
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s]\n{}", "z".repeat(3_000)),
            success: true,
        });
        conv.add_user_message("t1");

        compact_old_tool_results_in_place(&mut conv, 1, true);

        let get = |cid: &str| -> String {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        assert!(!get("r1").starts_with('['), "first read_file must be exempt");
        assert!(!get("r2").starts_with('['), "second read_file must also be exempt");
        assert!(get("b").starts_with("[bash "), "bash must still be stubbed");
    }

    #[test]
    fn compact_old_keep_one_recent_turn_keeps_last_turn_full() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        // Three turns, keep_recent_turns=1 → first two get stubbed, last stays full.
        for n in 0..3 {
            conv.add_user_message(&format!("t{n}"));
            let id = format!("b{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: id.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("[elapsed: 0.{}s, exit: 0]\n{}", n, "x".repeat(3_000)),
                success: true,
            });
        }

        compact_old_tool_results_in_place(&mut conv, 1, false);
        let get = |cid: &str| -> String {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        assert!(get("b0").starts_with("[bash "), "b0 must be stubbed");
        assert!(get("b1").starts_with("[bash "), "b1 must be stubbed");
        assert!(!get("b2").starts_with("[bash "), "b2 (last turn) must stay full");
    }

    #[test]
    fn compact_old_keep_zero_recent_turns_stubs_all() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        // Three turns, keep_recent_turns=0 → ALL turns get stubbed.
        for n in 0..3 {
            conv.add_user_message(&format!("t{n}"));
            let id = format!("b{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: id.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("[elapsed: 0.{}s, exit: 0]\n{}", n, "x".repeat(3_000)),
                success: true,
            });
        }

        compact_old_tool_results_in_place(&mut conv, 0, false);
        assert!(conv.messages.iter().any(|m| match &m.content {
            MessageContent::ToolResult(r) => r.output.starts_with("[bash "),
            _ => false,
        }), "all bash results must be stubbed when keep_recent_turns=0");
    }

    #[test]
    fn collapse_committed_freezes_stubbed_prefix_across_turns() {
        use crate::tool::{ToolCall, ToolResult};
        // Low budget (threshold = 1_000*4*70/100 = 2_800 chars) on purpose:
        // after the first collapse shrinks the conv, the SECOND collapse must
        // still be above threshold so it actually re-fires and re-examines the
        // already-stubbed prefix — otherwise the freeze assertion is vacuous.
        let budget = 1_000;

        let add_turn = |conv: &mut Conversation, n: usize| {
            conv.add_user_message(&format!("task {n}"));
            let id = format!("c{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: id.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
                success: true,
            });
        };

        let mut conv = Conversation::new();
        for n in 0..5 {
            add_turn(&mut conv, n);
        }
        collapse_committed(&mut conv, budget);

        // Snapshot every ALREADY-stubbed tool result (output starts with '[' and
        // is a real stub, i.e. contains "lines, first:").
        let stubbed_before: Vec<(usize, String)> = conv
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match &m.content {
                MessageContent::ToolResult(r)
                    if r.output.starts_with('[') && r.output.contains("lines, first:") =>
                {
                    Some((i, r.output.clone()))
                }
                _ => None,
            })
            .collect();
        assert!(!stubbed_before.is_empty(), "expected stubs after first collapse");

        // A new turn arrives; collapse again (the just-aged turn now stubs).
        add_turn(&mut conv, 5);
        collapse_committed(&mut conv, budget);

        // Every previously-stubbed result must be byte-identical (monotonic, frozen).
        for (i, before) in &stubbed_before {
            match &conv.messages[*i].content {
                MessageContent::ToolResult(r) => assert_eq!(
                    &r.output, before,
                    "stub at message #{i} mutated across turns — prefix cache would break"
                ),
                other => panic!("message #{i} changed content variant: {:?}", other),
            }
        }
    }
}
