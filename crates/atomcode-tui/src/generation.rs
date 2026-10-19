//! Ephemeral stream throughput. The host supplies time; replay does not invent it.
use crate::moment::Timestamp;
use atomcode_harness::session::SessionEvent;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Generation {
    request: Option<(u64, u32)>,
    phase: Option<bool>,
    started: Option<Timestamp>,
    chars: u64,
}

impl Generation {
    pub fn absorb(&mut self, event: &SessionEvent, now: Timestamp) {
        match event {
            SessionEvent::RequestHeader { turn, round, .. } => {
                *self = Self {
                    request: Some((*turn, *round)),
                    ..Self::default()
                };
            }
            SessionEvent::AssistantChunk {
                turn,
                round,
                delta,
                reasoning,
            } if self.request == Some((*turn, *round)) && !delta.is_empty() => {
                if self.phase != Some(*reasoning) {
                    self.phase = Some(*reasoning);
                    self.started = Some(now);
                    self.chars = 0;
                }
                self.chars = self.chars.saturating_add(delta.chars().count() as u64);
            }
            SessionEvent::Notice {
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                ..
            } => {
                self.phase = None;
                self.started = None;
                self.chars = 0;
            }
            SessionEvent::AssistantMessage { turn, round, .. }
                if self.request == Some((*turn, *round)) =>
            {
                *self = Self::default();
            }
            SessionEvent::TurnStart { .. } | SessionEvent::TurnEnd { .. } => {
                *self = Self::default();
            }
            _ => {}
        }
    }

    /// Same rough four characters/token estimate as classic tuix, never billing usage.
    pub fn tokens(&self) -> u64 {
        self.chars / 4
    }

    pub fn rate(&self, now: Timestamp) -> Option<u64> {
        let ms = now.as_millis().saturating_sub(self.started?.as_millis());
        (ms >= 1_000 && self.tokens() > 0)
            .then(|| (self.tokens() as f64 * 1_000.0 / ms as f64).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    #[test]
    fn speed_counts_current_phase_and_excludes_waiting_and_old_requests() {
        let mut g = Generation::default();
        g.absorb(&header(1), Timestamp::millis(0));
        assert_eq!(g.rate(Timestamp::millis(60_000)), None);
        g.absorb(&chunk(1, true, &"a".repeat(400)), Timestamp::millis(60_000));
        assert_eq!(g.tokens(), 100);
        assert_eq!(g.rate(Timestamp::millis(60_999)), None);
        assert_eq!(g.rate(Timestamp::millis(62_000)), Some(50));
        g.absorb(&chunk(1, false, "abcdefgh"), Timestamp::millis(63_000));
        assert_eq!(g.tokens(), 2);
        assert_eq!(g.rate(Timestamp::millis(64_000)), Some(2));
        g.absorb(&header(2), Timestamp::millis(90_000));
        g.absorb(&chunk(1, true, "late chunk"), Timestamp::millis(91_000));
        assert_eq!(g.tokens(), 0);
        assert_eq!(g.rate(Timestamp::millis(92_000)), None);
    }
    #[test]
    fn completed_generation_and_retry_clear_the_display() {
        let mut g = Generation::default();
        g.absorb(&header(1), Timestamp::millis(0));
        g.absorb(&chunk(1, true, "你好世界"), Timestamp::millis(100));
        assert_eq!(g.tokens(), 1, "count characters, not UTF-8 bytes");
        g.absorb(
            &SessionEvent::Notice {
                turn: 1,
                notice: atomcode_harness::session::NoticeKind::ProviderRetry,
                detail: String::new(),
                retry: None,
            },
            Timestamp::millis(200),
        );
        assert_eq!(g.tokens(), 0);
        g.absorb(&chunk(1, false, "aabbccdd"), Timestamp::millis(10_000));
        assert_eq!(
            g.rate(Timestamp::millis(11_000)),
            Some(2),
            "retry may resume within the same request header"
        );
        g.absorb(
            &SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: String::new(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_blocks: vec![],
                meta: None,
            },
            Timestamp::millis(11_000),
        );
        assert_eq!(g.tokens(), 0);
        g.absorb(
            &SessionEvent::TurnStart { turn: 2 },
            Timestamp::millis(12_000),
        );
        assert_eq!(g.tokens(), 0);
        assert_eq!(g.rate(Timestamp::millis(2_000)), None);
    }
}
