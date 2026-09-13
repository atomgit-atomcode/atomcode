//! History compaction written by the utility model.
//!
//! `compaction-tail` (in `loop_policy.rs`) folds the history away by *listing*
//! what happened, model-free. That is the right default — a compactor that needs
//! a model call cannot run when the provider is the thing that is failing — but a
//! written summary carries far more of the session's substance for the same
//! number of tokens.
//!
//! This row is the other half of the same seam. It folds the *same* span (the
//! boundary is `settled_span`, shared with the model-free strategy so the two
//! cannot disagree about what was compacted) and asks `llm-utility` to rewrite
//! the digest as a summary. Every way the model can fail to help — no utility
//! provider mounted, the call errors, it times out, it answers nothing — falls
//! back to the model-free text, so a compaction always produces a summary.
//!
//! Opt-in, by patch: one `[[remove]]` of `compaction-tail` and one `[[insert]]`
//! of `compaction-summary` (plus a provider for `llm-utility`). One seam has one
//! provider, so this replaces that row rather than joining it.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::StreamEvent;
use atomcode_plexus::{Context, Plugin};
use futures::StreamExt;
use serde_json::Value;

use crate::seams::{Compaction, CompactionDecision, CompactionSvc, LlmUtilitySvc};

use super::loop_policy;

fn parse<T: for<'de> serde::Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

const SUMMARY_SYSTEM: &str = "You compress the earlier part of an engineering session into a \
short handover note for whoever continues it. Keep what a continuation needs: what was asked, \
decisions made, files and commands that mattered, and anything still unresolved. Drop \
pleasantries and repetition. Write plainly, in the language of the session, with no preamble \
and no closing offer to help.";

/// The header the model-free strategy uses. Kept here too so a fallback reads the
/// same as the strategy it replaced — the shape of a compacted block is not this
/// row's to change.
const FALLBACK_HEADER: &str =
    "=== EARLIER IN THIS SESSION ===\nThese turns were compacted. What was asked:\n";
const FALLBACK_FOOTER: &str =
    "Ask again for any detail you need from before this point rather than assuming it.\n";

/// A model-written summary, with the model-free list as its floor.
struct SummarisingCompaction {
    ctx: Context,
    keep_turns: u64,
    max_tokens: u32,
    timeout: Duration,
}

impl SummarisingCompaction {
    /// Ask the utility model to summarize `digest`. `None` on any failure, so the
    /// caller falls back — a summary is never worth failing a turn over.
    async fn ask(&self, digest: &str) -> Option<String> {
        // Resolved per call, so a patch that swaps the utility model applies to
        // the next compaction without remounting this row.
        let provider = self.ctx.service::<LlmUtilitySvc>()?;
        let prompt = vec![
            Message::system(SUMMARY_SYSTEM),
            Message::user(digest.to_string()),
        ];
        let options = ChatOptions {
            max_tokens: Some(self.max_tokens),
            ..ChatOptions::default()
        };
        let call = async {
            let mut stream = provider.chat_stream(&prompt, &[], &options).await.ok()?;
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                if let StreamEvent::TextDelta(text) = event {
                    out.push_str(&text);
                }
            }
            let trimmed = out.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl Compaction for SummarisingCompaction {
    fn describe(&self) -> String {
        format!(
            "keep the last {} turn(s), summarize the rest with the utility model",
            self.keep_turns
        )
    }

    async fn compact(&self, log: &crate::session::SessionLog) -> Option<CompactionDecision> {
        let span = loop_policy::settled_span(log, self.keep_turns)?;
        let summary = match self.ask(&span.digest).await {
            Some(written) => format!("=== EARLIER IN THIS SESSION ===\n{written}\n"),
            // The floor. Same text the model-free row would have produced, so a
            // provider that is down degrades the *quality* of the summary and
            // nothing else.
            None => format!("{FALLBACK_HEADER}{}{FALLBACK_FOOTER}", span.digest),
        };
        Some(CompactionDecision {
            through: span.through,
            summary,
        })
    }
}

#[derive(Debug, serde::Deserialize)]
struct SummaryRow {
    #[serde(default = "default_keep")]
    keep_turns: u64,
    #[serde(default = "default_tokens")]
    max_tokens: u32,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    /// When to compact, shared with the model-free row so swapping the strategy
    /// does not change the pressure that triggers it.
    #[serde(default = "default_threshold")]
    threshold: f32,
}

fn default_keep() -> u64 {
    2
}
fn default_tokens() -> u32 {
    1024
}
fn default_timeout() -> u64 {
    60
}
fn default_threshold() -> f32 {
    0.75
}

impl Default for SummaryRow {
    fn default() -> Self {
        Self {
            keep_turns: default_keep(),
            max_tokens: default_tokens(),
            timeout_secs: default_timeout(),
            threshold: default_threshold(),
        }
    }
}

pub struct CompactionSummaryPlugin;

#[async_trait]
impl Plugin for CompactionSummaryPlugin {
    fn name(&self) -> &'static str {
        "compaction-summary"
    }
    fn uses(&self) -> &'static [&'static str] {
        // `compaction` because it fills that slot; `llm` for the context window
        // the trigger measures against; `llm-utility` for the summary itself.
        &["compaction", "llm", "llm-utility"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["compaction"]
    }
    fn description(&self) -> &'static str {
        "history compaction summarized by the utility model, falling back to the model-free list"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SummaryRow = parse(config)?;
        let _ = ctx
            .provide::<CompactionSvc>(Arc::new(SummarisingCompaction {
                ctx: ctx.clone(),
                keep_turns: row.keep_turns,
                max_tokens: row.max_tokens,
                timeout: Duration::from_secs(row.timeout_secs),
            }))
            .map_err(|e| e.to_string())?;
        // The same trigger the model-free row mounts. Without it a swapped-in
        // strategy would be one that never fires.
        loop_policy::mount_compaction_trigger(ctx, row.threshold);
        Ok(())
    }
}
