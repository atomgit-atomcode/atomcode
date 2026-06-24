//! Lifecycle-event synthesis for the native adapter.
//!
//! The kernel's terminal `TurnComplete { reason }` is LEAN — it carries only the
//! stop reason. The driver renders RICH terminal events (duration / token counts /
//! turn + tool tallies + the conversation snapshot the session persists from), so
//! the adapter synthesizes them from the per-turn [`TurnStats`] it accumulated over
//! the stream. This is the pure, testable core of the adapter's turn-end handling.

use atomcode_coding::TurnStats;
use atomcode_core::agent::{AgentPhase, TurnStopReason};
use atomcode_core::conversation::message::Message;
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_kernel::event::StopReason;

use super::event::UiEvent;

/// Synthesize the terminal UI events for a finished turn. `Cancelled` →
/// `TurnCancelled`; any other reason → a rich `TurnComplete` (stats from `stats`),
/// classified via [`TurnStopReason`]. Always trailed by `PhaseChange(Idle)`.
pub(crate) fn finish_turn_events(
    stats: &TurnStats,
    reason: StopReason,
    messages: Vec<Message>,
) -> Vec<UiEvent> {
    let snapshot = ConversationSnapshot { messages, cold_summaries: vec![] };
    let terminal = match reason {
        StopReason::Cancelled => UiEvent::TurnCancelled { snapshot },
        other => {
            let stop_reason = match other {
                StopReason::Stopped => TurnStopReason::Natural,
                StopReason::MaxRounds => TurnStopReason::TurnLimit,
                StopReason::MaxContinuations => TurnStopReason::StepLimit,
                StopReason::Cancelled => TurnStopReason::Cancelled, // unreachable (handled above)
                _ => TurnStopReason::Error,
            };
            UiEvent::TurnComplete {
                duration: stats.duration(),
                total_tokens: stats.total_tokens(),
                turn_count: stats.rounds(),
                tool_call_count: stats.tool_calls(),
                stop_reason,
                snapshot,
            }
        }
    };
    vec![terminal, UiEvent::PhaseChange(AgentPhase::Idle)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::conversation::message::Role;

    fn msgs() -> Vec<Message> {
        vec![Message::new(Role::User, "hi")]
    }

    fn stats_with(rounds_tokens: &[(usize, usize)], tools: usize) -> TurnStats {
        let mut s = TurnStats::new();
        for &(p, c) in rounds_tokens {
            s.on_usage(p, c);
        }
        for _ in 0..tools {
            s.on_tool_started();
        }
        s
    }

    #[test]
    fn cancelled_emits_turn_cancelled_then_idle() {
        let evs = finish_turn_events(&TurnStats::new(), StopReason::Cancelled, msgs());
        assert!(matches!(&evs[0], UiEvent::TurnCancelled { snapshot } if snapshot.messages.len() == 1));
        assert!(matches!(&evs[1], UiEvent::PhaseChange(AgentPhase::Idle)));
    }

    #[test]
    fn stopped_emits_rich_turn_complete_then_idle() {
        let stats = stats_with(&[(10, 5)], 1);
        let evs = finish_turn_events(&stats, StopReason::Stopped, msgs());
        assert!(matches!(&evs[0],
            UiEvent::TurnComplete { total_tokens, turn_count, tool_call_count, stop_reason, snapshot, .. }
            if *total_tokens == 15 && *turn_count == 1 && *tool_call_count == 1
                && matches!(stop_reason, TurnStopReason::Natural)
                && snapshot.messages.len() == 1));
        assert!(matches!(&evs[1], UiEvent::PhaseChange(AgentPhase::Idle)));
    }

    #[test]
    fn stop_reason_classification() {
        let cases = [
            (StopReason::MaxRounds, TurnStopReason::TurnLimit),
            (StopReason::MaxContinuations, TurnStopReason::StepLimit),
            (StopReason::ProviderError, TurnStopReason::Error),
            (StopReason::Timeout, TurnStopReason::Error),
            (StopReason::PromptRejected, TurnStopReason::Error),
        ];
        for (reason, expected) in cases {
            let evs = finish_turn_events(&TurnStats::new(), reason, msgs());
            match &evs[0] {
                UiEvent::TurnComplete { stop_reason, .. } => assert_eq!(
                    std::mem::discriminant(stop_reason),
                    std::mem::discriminant(&expected),
                    "{reason:?} should classify as {expected:?}"
                ),
                other => panic!("expected TurnComplete, got {other:?}"),
            }
        }
    }
}
