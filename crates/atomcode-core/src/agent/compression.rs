//! Context compression orchestration shared by CLI (AgentLoop) and daemon.
//!
//! Extracted from AgentLoop so both paths use the same compaction logic.
//! This module owns the "decision" layer: when to compress, what strategy
//! to apply, and how to verify success. The underlying render primitives
//! live in `crate::ctx::render`; the conversation data model lives in
//! `crate::conversation`.
//!
//! # Strategy
//!
//! [`maybe_compress_history`] implements a two-tier strategy:
//! 1. **Tier 1** — collapse old tool_results to one-line stubs (no LLM call).
//! 2. **Tier 2** — LLM-summarize old turns into the cold zone (FIFO, max 3).
//!
//! If different strategies are needed (e.g. daemon-only lightweight mode,
//! skip-LLM mode), compose the lower-level functions directly.

use crate::conversation::Conversation;
use crate::ctx::CtxBuilder;
use crate::provider::LlmProvider;
use crate::tool::ToolRegistry;

/// System prompt used when calling the LLM to summarize compression content.
pub const COMPRESSION_SYSTEM_PROMPT: &str =
    "You are a conversation summarizer. Output ONLY the summary.";

/// Result of a compression attempt.
#[derive(Debug, Clone, Copy)]
pub struct CompressionOutcome {
    pub applied: bool,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub removed_messages: usize,
}

/// Build a prompt for the LLM summarizer from mechanical turn summaries.
pub fn default_summarize_prompt(content: &str) -> String {
    format!(
        "Summarize this conversation history in 3-5 concise sentences. \
         Keep: file names, what was changed, key decisions, errors encountered. \
         Drop: exact code content, tool arguments, line numbers.\n\n{}",
        content
    )
}

/// Compute the token ceiling for the *kept tail* after compression.
///
/// Formula: `window - system - tool_defs - cold_zone - output_reserve`
/// where `output_reserve = (window / 4).clamp(8_000, 16_384)`.
/// Floor is `window / 4` — tail never shrinks below 25% of the window.
///
/// This is `async` because `ToolRegistry::get_definitions()` uses
/// `tokio::sync::RwLock` internally.
pub async fn compaction_keep_ceiling(
    ctx: &dyn CtxBuilder,
    system_prompt: &str,
    tools: &ToolRegistry,
    cold_summaries: &[String],
) -> usize {
    let window = ctx.ctx_window();
    let system_tokens = system_prompt.len() / 4 + 4;
    let tool_def_tokens: usize = tools
        .get_definitions()
        .await
        .iter()
        .map(|d| {
            let params = serde_json::to_string(&d.parameters).unwrap_or_default();
            (d.name.len() + d.description.len() + params.len()) / 4 + 4
        })
        .sum();
    let cold_zone_tokens: usize = cold_summaries.iter().map(|s| s.len() / 4 + 4).sum();
    let output_reserve = (window / 4).clamp(8_000, 16_384);
    window
        .saturating_sub(system_tokens)
        .saturating_sub(tool_def_tokens)
        .saturating_sub(cold_zone_tokens)
        .saturating_sub(output_reserve)
        .max(window / 4)
}

/// Run a lightweight LLM call to summarize content.
///
/// Returns `(summary, prompt_tokens_estimate, completion_tokens_estimate,
/// cached_tokens_estimate, had_error)`.
/// On failure, returns an empty string with `had_error = true` and logs the
/// error via `tracing::warn!`. Callers should fall back to the mechanical
/// summary when the returned string is empty.
///
/// Telemetry is NOT handled here — callers that need telemetry (AgentLoop)
/// should wrap this function with their own reporting logic.
pub async fn run_llm_summary(
    provider: &dyn LlmProvider,
    prompt: &str,
) -> (String, u32, u32, u32, bool) {
    let sys = COMPRESSION_SYSTEM_PROMPT;
    let mut mini_conv = Conversation::new();
    mini_conv.add_user_message(prompt);
    let msgs = mini_conv.to_provider_messages(sys);

    let mut input_estimate: u32 = 0;
    let mut output_estimate: u32 = 0;
    let mut cached_estimate: u32 = 0;
    let mut got_usage = false;
    let mut errored = false;

    let mut summary = String::new();
    match provider.chat_stream(&msgs, None) {
        Ok(mut stream) => {
            use futures::StreamExt;
            let first_timeout = std::time::Duration::from_secs(30);
            let stream_timeout = std::time::Duration::from_secs(30);
            let mut got_token = false;
            loop {
                let timeout = if got_token {
                    stream_timeout
                } else {
                    first_timeout
                };
                match tokio::time::timeout(timeout, stream.next()).await {
                    Ok(Some(Ok(crate::stream::StreamEvent::Delta(text)))) => {
                        got_token = true;
                        let clean = text
                            .replace("<think>", "")
                            .replace("</think>", "")
                            .replace("<|im_start|>", "")
                            .replace("<|im_end|>", "");
                        summary.push_str(&clean);
                    }
                    Ok(Some(Ok(crate::stream::StreamEvent::Usage(u)))) => {
                        input_estimate = input_estimate.saturating_add(u.prompt_tokens as u32);
                        output_estimate =
                            output_estimate.saturating_add(u.completion_tokens as u32);
                        cached_estimate =
                            cached_estimate.saturating_add(u.cached_tokens as u32);
                        got_usage = true;
                    }
                    Ok(Some(Ok(crate::stream::StreamEvent::Done { .. }))) => break,
                    Ok(Some(Ok(_))) => continue,
                    Ok(Some(Err(e))) => {
                        tracing::warn!(
                            "LLM summary stream error: {e}. Prompt was {} chars.",
                            prompt.len()
                        );
                        errored = true;
                        break;
                    }
                    // timeout (Err) or end-of-stream (Ok(None))
                    _ => {
                        if !got_token {
                            tracing::warn!(
                                "LLM summary timed out (no tokens received). \
                                 Prompt was {} chars.",
                                prompt.len()
                            );
                        }
                        break;
                    }
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "LLM summary provider error: {e}. Prompt was {} chars.",
                prompt.len()
            );
            errored = true;
        }
    }

    if !got_usage {
        input_estimate = ((sys.len() + prompt.len()) / 4) as u32;
        output_estimate = (summary.len() / 3) as u32;
    }
    let had_error = errored || (summary.trim().is_empty() && !got_usage);
    if had_error {
        tracing::warn!(
            "LLM summary failed or returned empty. input_est={input_estimate}, \
             output_est={output_estimate}"
        );
    }

    (summary, input_estimate, output_estimate, cached_estimate, had_error)
}

/// Compute the on-wire token count for the current conversation + system prompt.
///
/// Uses an empty `turn_reminder` — the count is used for before/after
/// relative comparison in [`try_apply_compression`], not for exact
/// wire-size prediction.
pub fn rendered_token_count(
    ctx: &dyn CtxBuilder,
    conv: &Conversation,
    system_prompt: &str,
) -> usize {
    ctx.build_messages(conv, system_prompt, "")
        .0
        .iter()
        .map(|m| m.estimate_tokens())
        .sum()
}

/// Apply a compression candidate only when it reduces the next request payload.
///
/// Snapshots conversation state before applying, measures the on-wire
/// token delta, and rolls back if the summary didn't actually shrink
/// the payload. This is the single success criterion shared by all
/// compression entry points.
///
/// `post_compress_state` — if `Some`, the string is injected as a
/// synthetic user message after compression (restores task context).
/// Callers should generate this value immediately before calling this
/// function to minimise staleness.
pub fn try_apply_compression(
    ctx: &dyn CtxBuilder,
    conv: &mut Conversation,
    system_prompt: &str,
    remove_count: usize,
    summary: String,
    post_compress_state: Option<&str>,
) -> CompressionOutcome {
    let before_msg_count = conv.messages.len();
    let before_tokens = rendered_token_count(ctx, conv, system_prompt);

    let msgs_snapshot = conv.messages.clone();
    let cold_snapshot = conv.cold_summaries.clone();
    let turns_snapshot = conv.turn_tracker.clone();

    conv.apply_compression(remove_count, summary);
    if let Some(state_msg) = post_compress_state {
        if !state_msg.is_empty() {
            conv.add_synthetic_user_message(state_msg);
        }
    }

    let after_tokens = rendered_token_count(ctx, conv, system_prompt);
    let removed_messages = before_msg_count.saturating_sub(conv.messages.len());

    if after_tokens >= before_tokens {
        conv.messages = msgs_snapshot;
        conv.cold_summaries = cold_snapshot;
        conv.turn_tracker = turns_snapshot;
        CompressionOutcome {
            applied: false,
            before_tokens,
            after_tokens,
            removed_messages: 0,
        }
    } else {
        CompressionOutcome {
            applied: true,
            before_tokens,
            after_tokens,
            removed_messages,
        }
    }
}

/// Main compression entry point — two-tier strategy.
///
/// 1. **Tier 1**: collapse old tool_results to stubs (keep last 3 turns).
///    No LLM call. If sufficient, return early.
/// 2. **Tier 2**: build mechanical summaries via [`crate::ctx::render::build_compression_content`],
///    refine with an LLM call via [`run_llm_summary`], and apply via
///    [`try_apply_compression`].
///
/// `post_compress_state` — optional task-state message injected after
/// successful compression (see [`try_apply_compression`]).
pub async fn maybe_compress_history(
    ctx: &dyn CtxBuilder,
    conv: &mut Conversation,
    provider: &dyn LlmProvider,
    tools: &ToolRegistry,
    system_prompt: &str,
    post_compress_state: Option<&str>,
) {
    let sys_tokens = system_prompt.len() / 4 + 4;
    if !ctx.needs_compression(conv, sys_tokens) {
        return;
    }

    // ── Tier 1: collapse old tool_results (no LLM call) ──
    crate::ctx::render::compact_old_tool_results_in_place(conv, /* keep_recent_turns */ 3, false);

    // Re-check: if Tier 1 was enough, stop here.
    if !ctx.needs_compression(conv, sys_tokens) {
        return;
    }

    // ── Tier 2: LLM-summarize oldest turns into cold zone ──
    // Read everything we need from conv BEFORE the .await boundary so
    // the &mut conv borrow is released.
    let keep_ceiling = compaction_keep_ceiling(ctx, system_prompt, tools, &conv.cold_summaries).await;
    let plan = ctx.compression_plan(conv, keep_ceiling);
    let (content, n_msgs) = match plan {
        Some(p) => p,
        None => return,
    };

    let summarize_prompt = default_summarize_prompt(&content);

    let (summary, _in_tok, _out_tok, _cached_tok, _had_error) =
        run_llm_summary(provider, &summarize_prompt).await;
    let final_summary = if summary.trim().is_empty() {
        content
    } else {
        summary
    };

    let _ = try_apply_compression(ctx, conv, system_prompt, n_msgs, final_summary, post_compress_state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_summarize_prompt_includes_content() {
        let content = "User asked about file X. Agent ran bash to list directory. Found 3 results.";
        let prompt = default_summarize_prompt(content);
        assert!(prompt.contains(content), "prompt must include the original content");
        assert!(prompt.contains("Summarize"), "prompt must start with the instruction");
        assert!(prompt.contains("3-5 concise sentences"), "prompt must specify length");
        assert!(prompt.contains("file names"), "prompt must ask for file names");
        assert!(prompt.contains("key decisions"), "prompt must ask for decisions");
        assert!(prompt.contains("what was changed"), "prompt must ask for changes");
    }

    #[test]
    fn default_summarize_prompt_rejects_exact_code() {
        let prompt = default_summarize_prompt("some content");
        assert!(prompt.contains("Drop:"), "prompt must have Drop section");
        assert!(prompt.contains("exact code content"), "prompt must exclude exact code");
        assert!(prompt.contains("tool arguments"), "prompt must exclude tool arguments");
    }

    #[test]
    fn compression_outcome_applied_vs_rejected() {
        let applied = CompressionOutcome {
            applied: true,
            before_tokens: 10_000,
            after_tokens: 5_000,
            removed_messages: 10,
        };
        assert!(applied.applied);
        assert!(applied.before_tokens > applied.after_tokens);
        assert_eq!(applied.removed_messages, 10);

        let rejected = CompressionOutcome {
            applied: false,
            before_tokens: 5_000,
            after_tokens: 6_000,
            removed_messages: 0,
        };
        assert!(!rejected.applied);
        assert!(rejected.before_tokens < rejected.after_tokens);
        assert_eq!(rejected.removed_messages, 0);
    }

    #[test]
    fn compression_outcome_no_change() {
        let outcome = CompressionOutcome {
            applied: false,
            before_tokens: 5_000,
            after_tokens: 5_000,
            removed_messages: 0,
        };
        assert!(!outcome.applied);
        assert_eq!(outcome.before_tokens, outcome.after_tokens);
    }

    #[test]
    fn compression_system_prompt_is_concise() {
        assert!(!COMPRESSION_SYSTEM_PROMPT.is_empty(), "must not be empty");
        assert!(
            COMPRESSION_SYSTEM_PROMPT.len() < 200,
            "system prompt must be short: got {} chars",
            COMPRESSION_SYSTEM_PROMPT.len()
        );
        assert!(
            COMPRESSION_SYSTEM_PROMPT.contains("summar"),
            "must contain summarizer instruction"
        );
    }
}
