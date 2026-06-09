use crate::tool::ToolCall;

/// Default threshold for `RunawayDetector` — number of consecutive
/// identical chars that count as a runaway. Picked to leave normal
/// content alone (longest legitimate repeat we've observed in code
/// dumps is ~200 underscores in a horizontal rule) while still
/// catching a degenerate model that's repeated a single token for
/// kilobytes (#225).
pub const DEFAULT_RUNAWAY_THRESHOLD: usize = 1024;

/// Stream-side guard that flags a degenerate `Delta` sequence — a
/// reasoning-model gets stuck in an autoregressive loop and starts
/// emitting the same character forever, eating tokens until the
/// caller's quota runs out (atomgit issue #225).
///
/// Feed each visible `Delta` text into `feed`; the detector returns
/// `Some(reason)` as soon as `threshold` consecutive identical chars
/// have arrived across deltas. State persists between calls, so a
/// runaway split across many small deltas still trips at the same
/// point as one giant delta would. Returning `None` means "no
/// runaway yet, keep going".
#[derive(Debug, Clone)]
pub struct RunawayDetector {
    last_char: Option<char>,
    repeat_count: usize,
    threshold: usize,
}

impl RunawayDetector {
    pub fn new(threshold: usize) -> Self {
        Self {
            last_char: None,
            repeat_count: 0,
            threshold: threshold.max(1),
        }
    }

    pub fn with_default_threshold() -> Self {
        Self::new(DEFAULT_RUNAWAY_THRESHOLD)
    }

    /// Feed a delta. Returns `Some(reason)` on the first delta that
    /// pushes the running repeat count over the threshold. After
    /// that the detector keeps state but will only re-fire once the
    /// repeat character changes and the count climbs again.
    pub fn feed(&mut self, text: &str) -> Option<String> {
        for c in text.chars() {
            if self.last_char == Some(c) {
                self.repeat_count = self.repeat_count.saturating_add(1);
                if self.repeat_count == self.threshold {
                    return Some(format!(
                        "model emitted {:?} {} times in a row — looks like an autoregressive runaway, aborting stream",
                        c, self.repeat_count
                    ));
                }
            } else {
                self.last_char = Some(c);
                self.repeat_count = 1;
            }
        }
        None
    }

    /// Drop any accumulated state (e.g. after a tool-call boundary
    /// where the visible text channel resets).
    pub fn reset(&mut self) {
        self.last_char = None;
        self.repeat_count = 0;
    }
}

#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Tokens served from provider's prompt cache (0 if not supported).
    pub cached_tokens: usize,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    /// Reasoning-model thinking content (e.g. MiniMax-M2.7, DeepSeek-R1,
    /// o1-series). Some OpenAI-compatible gateways route the full response
    /// here when `content` is empty — `TurnRunner` promotes it to the
    /// final text on `Done` if `content` ends up empty, which keeps us from
    /// silently returning 0-token "Nailed it" responses for reasoning models.
    Reasoning(String),
    /// One complete Anthropic extended-thinking content block. Emitted at
    /// `content_block_stop` by claude.rs after both `thinking_delta` and
    /// `signature_delta` have streamed in for a given block. The runner
    /// accumulates these into `MessageContent::AssistantWithToolCalls.
    /// thinking_blocks` so the next request can echo them back verbatim
    /// (Anthropic 400s otherwise: `The content[].thinking in the thinking
    /// mode must be passed back to the API`). Atomic per-block (vs
    /// streaming text + signature separately) keeps the runner from
    /// having to pair up out-of-order deltas across content blocks.
    ThinkingBlock {
        text: String,
        signature: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta(String),
    ToolCallDone(ToolCall),
    Usage(TokenUsage),
    /// Stream finished. `truncated` = true means finish_reason was "length"
    /// (model hit max_tokens and was cut off, should continue).
    Done {
        truncated: bool,
    },
    Error(String),
    /// Non-fatal advisory the provider wants surfaced to the user. Unlike
    /// `Error`, the stream and the turn continue normally — the warning
    /// is a heads-up (e.g. "your proxy looks like it's truncating
    /// input"), not a failure. The runner forwards it to
    /// `TurnEvent::Warning` so the TUI can render it without aborting.
    Warning(String),
}

#[cfg(test)]
mod runaway_tests {
    use super::*;

    #[test]
    fn normal_prose_never_trips() {
        let mut d = RunawayDetector::new(1024);
        assert_eq!(d.feed("Here is a perfectly fine response with no repetition."), None);
        assert_eq!(d.feed(" Even with some emphasis. Even with     spaces."), None);
    }

    #[test]
    fn long_run_of_same_char_trips_at_threshold() {
        let mut d = RunawayDetector::new(8);
        // First seven 'x' chars are fine.
        assert!(d.feed("xxxxxxx").is_none());
        // Eighth one trips.
        let reason = d.feed("x").expect("eighth x should trip");
        assert!(reason.contains("'x'"));
        assert!(reason.contains("8 times"));
    }

    #[test]
    fn runaway_split_across_deltas_still_trips() {
        let mut d = RunawayDetector::new(5);
        assert!(d.feed("aa").is_none());
        assert!(d.feed("aa").is_none());
        assert!(d.feed("a").expect("split runaway must still trip").contains("'a'"));
    }

    #[test]
    fn switching_char_resets_count() {
        let mut d = RunawayDetector::new(4);
        assert!(d.feed("aaa").is_none());
        // Switch to 'b' resets the run.
        assert!(d.feed("b").is_none());
        assert!(d.feed("bb").is_none());
        // Still under threshold (3 b's so far).
        assert!(d.feed("b").expect("4th b trips").contains("'b'"));
    }

    #[test]
    fn reset_clears_state() {
        let mut d = RunawayDetector::new(3);
        assert!(d.feed("aa").is_none());
        d.reset();
        // After reset, two more 'a's should not trip even though
        // total a's seen across the lifetime would be four.
        assert!(d.feed("aa").is_none());
    }

    #[test]
    fn unicode_safe() {
        let mut d = RunawayDetector::new(3);
        // Three identical emoji chars trip.
        let reason = d.feed("🌀🌀🌀").expect("emoji runaway");
        assert!(reason.contains("🌀"));
    }
}
