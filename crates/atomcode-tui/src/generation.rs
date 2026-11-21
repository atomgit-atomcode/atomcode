//! Ephemeral turn generation throughput. The host supplies time; replay invents none.
//!
//! Speed is decode speed: each request is timed from its first streamed output
//! to its last, so neither the wait for that first output (prefill, queueing)
//! nor a silence before the end frame is divided into it. Only output that
//! arrived after the first is counted against that window: whatever came with
//! the first output was generated before the window opened. Tokens are a local
//! estimate of what streamed: billed output is one total per request and
//! cannot say how much of it fell inside the window, so it only screens out
//! requests that streamed far less than they billed. The turn's requests are
//! aggregated (tokens summed, then divided by summed time).
use crate::moment::Timestamp;
use atomcode_harness::session::SessionEvent;

/// Shortest window over which one request contributes. A reply buffered and
/// delivered in one burst spans no time; one that arrives in a few frames
/// moments apart would read as thousands of tokens per second.
const MIN_REQUEST_MS: u64 = 1_000;
/// Evidence the turn must accumulate before any speed is shown, so a short
/// first reply cannot set a figure on its own.
const MIN_EVIDENCE_TOKENS: u64 = 200;
const MIN_EVIDENCE_MS: u64 = 2_000;
/// Billed output this many times the visible estimate means most of it was
/// never streamed (hidden reasoning). Where that generation fell relative to
/// the window is unknown, so the request contributes nothing rather than a
/// reading the estimate cannot vouch for.
const HIDDEN_RATIO: u64 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Request {
    id: (u64, u32),
    first: Option<Timestamp>,
    last: Option<Timestamp>,
    ended: Option<Timestamp>,
    /// Characters that tokenize roughly one apiece (CJK and full-width).
    wide: u64,
    /// Everything else, roughly four characters per token.
    narrow: u64,
    /// Estimated tokens that arrived with the first output.
    lead: u64,
    /// The provider's whole-request output count; screens, never counts.
    billed: Option<u64>,
    /// The attempt was discarded; its billing, if reported, is not this one's.
    discarded: bool,
}

impl Request {
    fn open(id: (u64, u32)) -> Self {
        Self {
            id,
            ..Self::default()
        }
    }

    fn push(&mut self, text: &str, now: Timestamp) {
        if text.is_empty() {
            return;
        }
        let first = *self.first.get_or_insert(now);
        self.last = Some(now);
        for ch in text.chars() {
            if is_wide(ch) {
                self.wide += 1;
            } else {
                self.narrow += 1;
            }
        }
        if first == now {
            self.lead = self.estimate();
        }
    }

    fn estimate(&self) -> u64 {
        self.wide + self.narrow / 4
    }

    /// `(tokens, ms)` this request contributes: the output after its first
    /// over the time from first to last output.
    fn reading(&self) -> Option<(u64, u64)> {
        let ms = self
            .last?
            .as_millis()
            .saturating_sub(self.first?.as_millis());
        if ms < MIN_REQUEST_MS {
            return None;
        }
        let estimate = self.estimate();
        if self
            .billed
            .is_some_and(|billed| billed > estimate.max(1) * HIDDEN_RATIO)
        {
            return None;
        }
        let tokens = estimate.saturating_sub(self.lead);
        (tokens > 0).then_some((tokens, ms))
    }
}

fn is_wide(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x11FF
        | 0x2E80..=0x9FFF
        | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x20000..=0x2FA1F)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    turn: Option<u64>,
    request: Option<Request>,
    /// Summed readings of the turn's earlier requests.
    done_tokens: u64,
    done_ms: u64,
}

impl Generation {
    pub fn absorb(&mut self, event: &SessionEvent, now: Timestamp) {
        match event {
            SessionEvent::TurnStart { turn } => {
                *self = Self {
                    turn: Some(*turn),
                    ..Self::default()
                };
            }
            SessionEvent::TurnEnd { turn, .. } if self.turn == Some(*turn) => {
                *self = Self::default();
            }
            SessionEvent::RequestHeader { turn, round, .. }
                if self.turn.is_none_or(|active| active == *turn) =>
            {
                let id = (*turn, *round);
                if self
                    .request
                    .as_ref()
                    .is_some_and(|request| request.id >= id)
                {
                    return;
                }
                if let Some((tokens, ms)) = self.request.as_ref().and_then(Request::reading) {
                    self.done_tokens += tokens;
                    self.done_ms += ms;
                }
                self.turn = Some(*turn);
                self.request = Some(Request::open(id));
            }
            SessionEvent::AssistantChunk {
                turn, round, delta, ..
            } => {
                if let Some(request) = self.current(*turn, *round) {
                    request.push(delta, now);
                }
            }
            SessionEvent::Usage { turn, round, usage } => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.id == (*turn, *round))
                {
                    if request.discarded && request.first.is_none() {
                        request.discarded = false;
                        return;
                    }
                    request.ended.get_or_insert(now);
                    request.billed = Some(u64::from(usage.completion));
                }
            }
            SessionEvent::AssistantMessage { turn, round, .. } => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.id == (*turn, *round))
                {
                    request.ended.get_or_insert(now);
                }
            }
            SessionEvent::ReasoningDiscarded { turn, round } => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.id == (*turn, *round))
                {
                    *request = Request {
                        discarded: true,
                        ..Request::open(request.id)
                    };
                }
            }
            SessionEvent::Notice {
                turn,
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                ..
            } if self.turn == Some(*turn) => {
                // The retry re-runs the current round. A failed attempt's
                // partial reply may already have ended it; discard that
                // attempt either way before measuring the retry.
                if let Some(request) = self.request.as_mut() {
                    *request = Request::open(request.id);
                }
            }
            _ => {}
        }
    }

    fn current(&mut self, turn: u64, round: u32) -> Option<&mut Request> {
        self.request
            .as_mut()
            .filter(|request| request.id == (turn, round) && request.ended.is_none())
    }

    /// The handle's live tool name/argument fragments share generation time
    /// with prose and reasoning. Metadata-only frames contribute no output.
    pub fn tool_stream(&mut self, name: Option<&str>, arguments: &str, now: Timestamp) -> bool {
        if name.is_none_or(str::is_empty) && arguments.is_empty() {
            return false;
        }
        let Some(request) = self.request.as_mut().filter(|r| r.ended.is_none()) else {
            return false;
        };
        request.push(name.unwrap_or_default(), now);
        request.push(arguments, now);
        true
    }

    /// Streaming token estimate for the current request.
    pub fn tokens(&self) -> u64 {
        self.request.as_ref().map_or(0, Request::estimate)
    }

    /// Decode speed over the turn's qualifying requests, including the one
    /// streaming now once it qualifies. It moves only with output: through
    /// tools, the next first-output wait or a silence before the end frame the
    /// earlier readings stand. A new turn or session clears this projection.
    pub fn rate(&self) -> Option<u64> {
        let (live_tokens, live_ms) = self
            .request
            .as_ref()
            .and_then(Request::reading)
            .unwrap_or_default();
        let tokens = self.done_tokens + live_tokens;
        let ms = self.done_ms + live_ms;
        (tokens >= MIN_EVIDENCE_TOKENS && ms >= MIN_EVIDENCE_MS)
            .then(|| (tokens as f64 * 1_000.0 / ms as f64).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::stream::TokenUsage;

    fn header(round: u32) -> SessionEvent {
        SessionEvent::RequestHeader {
            turn: 1,
            round,
            model: "m".into(),
            reason: atomcode_harness::session::HeaderReason::Append,
        }
    }
    fn chunk(round: u32, reasoning: bool, text: &str) -> SessionEvent {
        SessionEvent::AssistantChunk {
            turn: 1,
            round,
            delta: text.into(),
            reasoning,
        }
    }
    fn usage(round: u32, completion: u32) -> SessionEvent {
        SessionEvent::Usage {
            turn: 1,
            round,
            usage: TokenUsage {
                prompt: 100,
                completion,
                cached: 0,
            },
        }
    }
    fn message(round: u32) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn: 1,
            round,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_blocks: vec![],
            meta: None,
        }
    }
    fn retry() -> SessionEvent {
        SessionEvent::Notice {
            turn: 1,
            notice: atomcode_harness::session::NoticeKind::ProviderRetry,
            detail: String::new(),
            retry: None,
        }
    }
    fn at(g: &mut Generation, event: SessionEvent, ms: u64) {
        g.absorb(&event, Timestamp::millis(ms));
    }
    /// `n` narrow characters: `n / 4` estimated tokens.
    fn text(n: usize) -> String {
        "a".repeat(n)
    }

    #[test]
    fn the_wait_for_first_output_is_not_divided_into_the_speed() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        assert_eq!(g.rate(), None, "a minute of prefill is no reading");
        at(&mut g, chunk(1, true, &text(4)), 60_000);
        at(&mut g, chunk(1, false, &text(1_600)), 62_000);
        // 400 tokens after the first output over the 2s since it, not 62s.
        assert_eq!(g.rate(), Some(200));
        at(&mut g, usage(1, 520), 64_000);
        at(&mut g, message(1), 64_000);
        assert_eq!(
            g.rate(),
            Some(200),
            "the end frame does not move the reading"
        );
        at(&mut g, header(2), 90_000);
        assert_eq!(
            g.rate(),
            Some(200),
            "tools and the next wait are not counted"
        );
    }

    #[test]
    fn output_that_came_with_the_first_frame_is_not_counted_in_the_window() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        // 900 tokens generated before the window opened, 200 inside it.
        at(&mut g, chunk(1, false, &text(3_600)), 10_000);
        at(&mut g, chunk(1, false, &text(800)), 12_000);
        assert_eq!(g.rate(), Some(100), "not (900 + 200) / 2s");
        // Billing is one total: its excess may be hidden reasoning from before
        // the window, so none of it is attributed to the window.
        at(&mut g, usage(1, 3_000), 12_000);
        assert_eq!(g.rate(), Some(100));
    }

    #[test]
    fn a_reply_delivered_in_one_burst_yields_no_reading() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4_000)), 60_000);
        at(&mut g, usage(1, 1_000), 60_050);
        assert_eq!(g.rate(), None);
        at(&mut g, header(2), 70_000);
        at(&mut g, chunk(2, false, &text(4_000)), 80_000);
        at(&mut g, chunk(2, false, &text(4_000)), 80_400);
        at(&mut g, usage(2, 2_000), 80_400);
        assert_eq!(g.rate(), None, "frames moments apart are a burst too");
    }

    #[test]
    fn a_burst_followed_by_a_late_end_frame_yields_no_reading() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4_000)), 60_000);
        assert_eq!(g.rate(), None);
        // Nothing streamed in the 2.5s before the end frame.
        at(&mut g, usage(1, 1_000), 62_500);
        at(&mut g, message(1), 62_500);
        assert_eq!(g.rate(), None);
    }

    #[test]
    fn billing_far_above_what_streamed_yields_no_reading() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4)), 10_000);
        at(&mut g, chunk(1, false, &text(800)), 12_000);
        assert_eq!(g.rate(), Some(100));
        // Most of the output was never streamed: the window started late.
        at(&mut g, usage(1, 1_000), 13_000);
        assert_eq!(g.rate(), None);
    }

    #[test]
    fn wide_characters_count_one_token_each() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, "你好，世界abcd"), 100);
        assert_eq!(g.tokens(), 6, "five wide characters plus four narrow ones");
    }

    #[test]
    fn short_replies_count_once_the_turn_has_enough_evidence() {
        let mut g = Generation::default();
        at(&mut g, SessionEvent::TurnStart { turn: 1 }, 0);
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4)), 10_000);
        at(&mut g, chunk(1, false, &text(600)), 11_500);
        at(&mut g, usage(1, 151), 11_500);
        assert_eq!(g.rate(), None, "150 tokens over 1.5s is not enough alone");
        at(&mut g, header(2), 20_000);
        at(&mut g, chunk(2, false, &text(4)), 30_000);
        at(&mut g, chunk(2, false, &text(600)), 31_500);
        assert_eq!(g.rate(), Some(100), "300 tokens over 3s");
    }

    #[test]
    fn the_turn_aggregates_requests_by_duration() {
        let mut g = Generation::default();
        at(&mut g, SessionEvent::TurnStart { turn: 1 }, 0);
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4)), 10_000);
        at(&mut g, chunk(1, false, &text(1_600)), 14_000);
        at(&mut g, usage(1, 401), 14_000);
        at(&mut g, header(2), 20_000);
        at(&mut g, chunk(2, false, &text(4_000)), 50_000);
        at(&mut g, usage(2, 1_000), 50_010);
        at(&mut g, header(3), 60_000);
        at(&mut g, chunk(3, false, &text(4)), 70_000);
        at(&mut g, chunk(3, false, &text(2_400)), 72_000);
        at(&mut g, usage(3, 601), 72_000);
        // (400 + 600) tokens over (4s + 2s); the burst contributes nothing.
        assert_eq!(g.rate(), Some(167));
        at(&mut g, SessionEvent::TurnStart { turn: 2 }, 90_000);
        assert_eq!(g.rate(), None, "a new turn clears the reading");
    }

    #[test]
    fn a_retry_after_a_committed_partial_reply_is_measured_afresh() {
        // Stream recovery commits the failed attempt's partial reply for the
        // same round, then announces the retry, which reuses that round.
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, &text(4_000)), 10_000);
        at(&mut g, chunk(1, false, &text(4_000)), 11_000);
        at(&mut g, message(1), 12_000);
        at(&mut g, retry(), 12_000);
        assert_eq!(g.tokens(), 0);
        assert_eq!(g.rate(), None);
        at(&mut g, chunk(1, false, &text(4)), 20_000);
        at(&mut g, chunk(1, false, &text(3_200)), 24_000);
        at(&mut g, usage(1, 801), 24_100);
        at(&mut g, message(1), 24_100);
        assert_eq!(g.rate(), Some(200), "the retry's own window and billing");
    }

    #[test]
    fn a_discarded_attempt_restarts_the_measurement() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, true, &text(4_000)), 1_000);
        at(&mut g, chunk(1, true, &text(4_000)), 4_000);
        at(
            &mut g,
            SessionEvent::ReasoningDiscarded { turn: 1, round: 1 },
            5_000,
        );
        // The discarded attempt's billing must not end the retry.
        at(&mut g, usage(1, 5_000), 5_000);
        assert_eq!(g.rate(), None);
        at(&mut g, chunk(1, false, &text(4)), 20_000);
        at(&mut g, chunk(1, false, &text(1_600)), 24_000);
        at(&mut g, usage(1, 401), 24_000);
        assert_eq!(g.rate(), Some(100));
    }

    #[test]
    fn replay_without_streamed_output_invents_no_speed() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, usage(1, 1_000), 0);
        at(&mut g, message(1), 0);
        assert_eq!(g.rate(), None);
        at(&mut g, chunk(1, false, &text(4_000)), 100_000);
        at(&mut g, chunk(1, false, &text(4_000)), 200_000);
        assert_eq!(g.rate(), None, "late chunks after the end are ignored");
    }

    #[test]
    fn tool_fragments_and_text_share_one_window() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        assert!(!g.tool_stream(None, "", Timestamp::millis(50_000)));
        at(&mut g, chunk(1, true, &text(4)), 60_000);
        assert!(g.tool_stream(Some("f"), &text(799), Timestamp::millis(62_000)));
        assert_eq!(g.rate(), Some(100));
        at(&mut g, usage(1, 201), 62_000);
        assert_eq!(g.rate(), Some(100));
        assert!(!g.tool_stream(Some("late"), "late", Timestamp::millis(99_000)));
    }
}
