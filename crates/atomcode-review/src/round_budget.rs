//! Round-budget pressure: make the reviewer LAND findings before the round fuse trips.
//!
//! The failure this fixes: on a large/complex diff the reviewer can spend its ENTIRE round
//! budget in a shallow read/grep exploration loop, never calling `report_finding` and never
//! emitting a text-only stop. It then hits the `max_rounds` fuse with ZERO findings and the
//! whole review is scored as a hard failure ("no findings collected") — even though the
//! model saw the problem, it never committed it before running out of rounds.
//!
//! The model has no innate awareness of its remaining round budget. This hook projects it:
//! once rounds cross a threshold it appends an EPHEMERAL reminder (a synthetic user turn,
//! never stored — see [`pre_request`](atomcode_kernel::hook::LifecycleHooks::pre_request))
//! telling the model to stop broad exploration and report what it already has, escalating to
//! a hard "final round" nudge on the last executing round. It fabricates no findings; it only
//! forces the model to commit the ones it already believes in before the cutoff.
//!
//! Only active when `max_rounds` is set (engineering callers pass `--max-rounds`); a bare
//! unbounded CLI run has no budget to pressure against, so the hook is a no-op there.

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;

/// Fraction of the round budget after which the soft "start landing findings" reminder
/// begins firing every round. `0.7` ⇒ the last ~30% of rounds carry pressure.
const DEFAULT_THRESHOLD: f32 = 0.7;

/// Appends an escalating "report now, stop exploring" reminder as the round budget runs down.
/// See the module docs for the why.
pub struct RoundBudgetHook {
    /// Pressure begins once `round >= threshold * max_rounds`. In `(0.0, 1.0]`.
    threshold: f32,
}

impl RoundBudgetHook {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

impl Default for RoundBudgetHook {
    fn default() -> Self {
        Self::new()
    }
}

/// The last-round hard nudge: the review ends after this round, so report everything now.
fn final_reminder(round: u32, max: u32) -> String {
    format!(
        "[review budget] FINAL round ({round} of {max}). This is your last chance to report. \
         Call `report_finding` for every issue you have not yet reported NOW, then write your \
         closing summary. After this round the review is cut off and any unreported finding is \
         lost — do not start new exploration."
    )
}

/// The soft nudge fired every round inside the pressure zone: commit confident findings,
/// only read further to confirm a specific suspicion.
fn soft_reminder(round: u32, max: u32) -> String {
    format!(
        "[review budget] You have used round {round} of {max}. Stop broad exploration. For \
         every issue you are ALREADY confident about, call `report_finding` immediately, then \
         write your closing summary. Only open a new file when it directly confirms a specific \
         suspected issue — the review is cut off at round {max} and unreported findings are lost."
    )
}

#[async_trait]
impl LifecycleHooks for RoundBudgetHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        // No budget → nothing to pressure against.
        let Some(max) = ctx.max_rounds else { return };
        if max == 0 {
            return;
        }
        let round = ctx.round;
        // `pre_request` only runs for round in 1..=max (the fuse returns before it at
        // round > max), so `round >= max` uniquely identifies the last executing round.
        // A one-line stderr trace (same channel as the CLI's `[rules]`/`[scope]` lines)
        // makes the pressure observable — you can see exactly when landing was forced.
        if round >= max {
            eprintln!("[budget] round {round}/{max}: injected FINAL landing reminder");
            messages.push(Message::synthetic_user(final_reminder(round, max)));
        } else if round as f32 >= max as f32 * self.threshold {
            eprintln!("[budget] round {round}/{max}: injected soft landing reminder");
            messages.push(Message::synthetic_user(soft_reminder(round, max)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::{Message, Role};

    fn ctx(round: u32, max_rounds: Option<u32>) -> TurnCtx {
        TurnCtx {
            round,
            max_rounds,
            ..Default::default()
        }
    }

    async fn run(round: u32, max_rounds: Option<u32>) -> Vec<Message> {
        let mut msgs = vec![
            Message::user("Review this diff"),
            Message::assistant("", vec![]),
        ];
        RoundBudgetHook::new()
            .pre_request(&mut msgs, &ctx(round, max_rounds))
            .await;
        msgs
    }

    #[tokio::test]
    async fn no_pressure_before_threshold() {
        // 27/40 = 0.675 < 0.70 → untouched.
        let msgs = run(27, Some(40)).await;
        assert_eq!(msgs.len(), 2, "no reminder appended below threshold");
    }

    #[tokio::test]
    async fn soft_pressure_at_threshold() {
        // 28/40 = 0.70 → soft reminder appended at the tail.
        let msgs = run(28, Some(40)).await;
        assert_eq!(msgs.len(), 3, "one reminder appended");
        let last = msgs.last().unwrap();
        assert_eq!(last.role, Role::User, "reminder is a user-role turn");
        assert!(
            last.synthetic,
            "reminder is synthetic (ephemeral, budget-injected)"
        );
        assert!(last.text.contains("report_finding"), "{}", last.text);
        assert!(
            last.text.contains("28") && last.text.contains("40"),
            "{}",
            last.text
        );
        assert!(
            !last.text.contains("FINAL"),
            "not the final nudge yet: {}",
            last.text
        );
    }

    #[tokio::test]
    async fn hard_pressure_on_final_round() {
        // round == max → the last executing round → hard "final" nudge.
        let msgs = run(40, Some(40)).await;
        assert_eq!(msgs.len(), 3, "one reminder appended");
        let last = msgs.last().unwrap();
        assert!(
            last.text.contains("FINAL"),
            "final-round nudge: {}",
            last.text
        );
        assert!(last.text.contains("report_finding"), "{}", last.text);
    }

    #[tokio::test]
    async fn no_op_when_unbounded() {
        // No round fuse → no budget to pressure against, even at a huge round.
        let msgs = run(9999, None).await;
        assert_eq!(msgs.len(), 2, "unbounded run gets no reminder");
    }

    #[tokio::test]
    async fn no_op_when_max_is_zero() {
        // Degenerate cap: never divide/pressure against zero.
        let msgs = run(1, Some(0)).await;
        assert_eq!(msgs.len(), 2, "max_rounds=0 is a no-op");
    }
}
