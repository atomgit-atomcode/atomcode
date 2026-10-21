//! Ephemeral current-request generation throughput. The host supplies time; replay invents none.
use crate::moment::Timestamp;
use atomcode_harness::session::SessionEvent;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Request {
    id: (u64, u32),
    started: Option<Timestamp>,
    ended: Option<Timestamp>,
    chars: u64,
    completion: Option<u64>,
}

impl Request {
    fn reading(&self, now: Timestamp) -> Option<(u64, u64)> {
        let started = self.started?;
        let ms = self
            .ended
            .unwrap_or(now)
            .as_millis()
            .saturating_sub(started.as_millis());
        let tokens = self.completion.unwrap_or(self.chars / 4);
        (ms > 0).then_some((tokens, ms))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    turn: Option<u64>,
    request: Option<Request>,
    last_rate: Option<u64>,
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
                self.last_rate = self.rate(now);
                self.turn = Some(*turn);
                self.request = Some(Request {
                    id,
                    ..Request::default()
                });
            }
            SessionEvent::AssistantChunk {
                turn, round, delta, ..
            } => {
                if let Some(request) = self.request.as_mut().filter(|request| {
                    request.id == (*turn, *round) && request.ended.is_none() && !delta.is_empty()
                }) {
                    request.started.get_or_insert(now);
                    request.chars = request.chars.saturating_add(delta.chars().count() as u64);
                }
            }
            SessionEvent::Usage { turn, round, usage } => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.id == (*turn, *round))
                {
                    request.completion =
                        (usage.completion > 0).then_some(u64::from(usage.completion));
                    request.ended.get_or_insert(now);
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
            SessionEvent::Notice {
                turn,
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                ..
            } if self.turn == Some(*turn) => {
                if let Some(request) = self
                    .request
                    .as_mut()
                    .filter(|request| request.ended.is_none())
                {
                    // Discard the failed attempt before measuring the retry.
                    *request = Request {
                        id: request.id,
                        ..Request::default()
                    };
                }
            }
            _ => {}
        }
    }

    /// The handle's live tool name/argument fragments share generation time
    /// with prose and reasoning. Metadata-only frames do not start the clock.
    pub fn tool_stream(&mut self, name: Option<&str>, arguments: &str, now: Timestamp) -> bool {
        let chars = name.map_or(0, |name| name.chars().count()) + arguments.chars().count();
        if chars == 0 {
            return false;
        }
        if let Some(request) = self.request.as_mut().filter(|r| r.ended.is_none()) {
            request.started.get_or_insert(now);
            request.chars = request.chars.saturating_add(chars as u64);
            return true;
        }
        false
    }

    /// Streaming token estimate for the current request.
    pub fn tokens(&self) -> u64 {
        self.request.as_ref().map_or(0, |request| request.chars / 4)
    }

    /// Live generation, then the last finished measurement through tools and
    /// first-token waits. Finished durations are frozen; no tool/idle time enters
    /// the denominator. A new turn or session clears this ephemeral projection.
    pub fn rate(&self, now: Timestamp) -> Option<u64> {
        let measured = self.request.as_ref().and_then(|request| {
            let (tokens, ms) = request.reading(now)?;
            if tokens == 0 || (request.ended.is_none() && ms < 250) {
                return None;
            }
            Some((tokens as f64 * 1_000.0 / ms as f64).round() as u64)
        });
        measured.or(self.last_rate)
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
    fn at(g: &mut Generation, event: SessionEvent, ms: u64) {
        g.absorb(&event, Timestamp::millis(ms));
    }
    #[test]
    fn measures_each_request_and_retains_finished_speed_during_tools_and_waits() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        assert_eq!(g.rate(Timestamp::millis(60_000)), None);
        at(&mut g, chunk(1, true, &"a".repeat(400)), 60_000);
        assert_eq!(g.rate(Timestamp::millis(60_249)), None);
        assert_eq!(g.rate(Timestamp::millis(60_250)), Some(400));
        at(&mut g, chunk(1, false, &"a".repeat(400)), 61_000);
        assert_eq!(g.rate(Timestamp::millis(62_000)), Some(100));
        at(&mut g, usage(1, 300), 62_000);
        at(&mut g, message(1), 62_000);
        assert_eq!(g.rate(Timestamp::millis(90_000)), Some(150));
        at(&mut g, header(2), 90_000);
        assert_eq!(g.rate(Timestamp::millis(150_000)), Some(150));
        at(&mut g, chunk(1, true, "late chunk"), 150_000);
        at(&mut g, usage(1, 999), 150_000);
        at(&mut g, chunk(2, false, &"a".repeat(400)), 150_000);
        assert_eq!(g.rate(Timestamp::millis(152_000)), Some(50));
        at(&mut g, usage(2, 100), 154_000);
        assert_eq!(
            g.rate(Timestamp::millis(200_000)),
            Some(25),
            "finished speed excludes the subsequent tool and idle time"
        );
    }
    #[test]
    fn retry_discards_partial_attempt() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, false, "你好世界"), 100);
        assert_eq!(g.tokens(), 1, "characters, not UTF-8 bytes");
        at(&mut g, usage(1, 100), 2_100);
        at(&mut g, header(2), 3_000);
        at(&mut g, chunk(2, true, &"a".repeat(400)), 4_000);
        at(
            &mut g,
            SessionEvent::Notice {
                turn: 1,
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                detail: String::new(),
                retry: None,
            },
            5_000,
        );
        assert_eq!(g.tokens(), 0);
        assert_eq!(g.rate(Timestamp::millis(100_000)), Some(50));
        at(&mut g, chunk(2, false, &"a".repeat(400)), 100_000);
        at(&mut g, message(2), 102_000);
        // Late usage calibrates tokens without extending the finished interval.
        at(&mut g, usage(2, 300), 110_000);
        assert_eq!(g.rate(Timestamp::millis(120_000)), Some(150));
        at(&mut g, SessionEvent::TurnStart { turn: 2 }, 130_000);
        at(&mut g, usage(2, 999), 131_000);
        assert_eq!(g.rate(Timestamp::millis(140_000)), None);
    }
    #[test]
    fn replay_and_zero_duration_do_not_invent_speed() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, usage(1, 100), 0);
        at(&mut g, message(1), 0);
        assert_eq!(g.rate(Timestamp::millis(100_000)), None);
        at(&mut g, header(2), 100_000);
        at(&mut g, chunk(2, false, "abcd"), 101_000);
        at(&mut g, usage(2, 1), 101_000);
        assert_eq!(g.rate(Timestamp::millis(200_000)), None);
    }
    #[test]
    fn pure_tool_output_retains_its_finished_speed() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        assert!(!g.tool_stream(None, "", Timestamp::millis(50_000)));
        assert_eq!(g.rate(Timestamp::millis(59_000)), None);
        assert!(g.tool_stream(Some("read_file"), "{", Timestamp::millis(60_000)));
        g.tool_stream(None, &"a".repeat(400), Timestamp::millis(61_000));
        at(&mut g, usage(1, 94), 62_000);
        assert_eq!(g.rate(Timestamp::millis(63_000)), Some(47));
        assert!(!g.tool_stream(Some("late"), "late", Timestamp::millis(64_000)));
        at(&mut g, header(2), 70_000);
        assert_eq!(g.rate(Timestamp::millis(130_000)), Some(47));
    }

    #[test]
    fn tool_fragments_and_text_share_one_duration_and_final_only_stays_unknown() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        at(&mut g, chunk(1, true, &"a".repeat(400)), 10_000);
        g.tool_stream(Some("f"), &"b".repeat(399), Timestamp::millis(11_000));
        assert_eq!(g.rate(Timestamp::millis(12_000)), Some(100));
        at(&mut g, usage(1, 400), 12_000);
        assert_eq!(g.rate(Timestamp::millis(99_000)), Some(200));
        at(&mut g, SessionEvent::TurnStart { turn: 2 }, 100_000);
        at(
            &mut g,
            SessionEvent::RequestHeader {
                turn: 2,
                round: 1,
                model: "m".into(),
                reason: atomcode_harness::session::HeaderReason::Append,
            },
            100_000,
        );
        g.tool_stream(Some("f"), "{}", Timestamp::millis(120_000));
        at(
            &mut g,
            SessionEvent::Usage {
                turn: 2,
                round: 1,
                usage: TokenUsage {
                    prompt: 1,
                    completion: 94,
                    cached: 0,
                },
            },
            120_000,
        );
        assert_eq!(g.rate(Timestamp::millis(180_000)), None);
    }
    #[test]
    fn fast_tool_response_uses_real_usage_after_a_short_sampling_interval() {
        let mut g = Generation::default();
        at(&mut g, header(1), 0);
        g.tool_stream(Some("read_file"), "{", Timestamp::millis(60_000));
        assert_eq!(
            g.rate(Timestamp::millis(60_249)),
            None,
            "streaming estimates still need a stable interval"
        );
        at(&mut g, usage(1, 94), 60_500);
        assert_eq!(g.rate(Timestamp::millis(61_000)), Some(188));
        at(&mut g, header(2), 70_000);
        assert_eq!(g.rate(Timestamp::millis(120_000)), Some(188));
    }
}
