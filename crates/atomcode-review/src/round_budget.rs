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
//! Active for round or wall-clock budgets. Time pressure prevents a slow model from
//! reaching its deadline long before its round threshold. Unbounded runs remain unchanged.

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
    /// See `ReviewAgentConfig::trace_budget_to_stderr`.
    trace: bool,
    clock: Option<(tokio::time::Instant, std::time::Duration)>,
}

impl RoundBudgetHook {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            trace: false,
            clock: None,
        }
    }

    /// Use the caller's remaining wall-clock allowance as landing pressure,
    /// alongside the existing hard cancellation deadline.
    pub fn with_duration(mut self, duration: Option<std::time::Duration>) -> Self {
        self.clock = duration
            .filter(|d| !d.is_zero())
            .map(|d| (tokio::time::Instant::now(), d));
        self
    }

    /// Also say on stderr when landing is forced — for a caller that owns the
    /// terminal (the `atomcode review` command).
    pub fn tracing_to_stderr(mut self, trace: bool) -> Self {
        self.trace = trace;
        self
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
        if let Some((started, limit)) = self.clock {
            let elapsed = started.elapsed();
            let remaining = limit.saturating_sub(elapsed).as_secs();
            let pressure = elapsed >= std::time::Duration::from_secs(300)
                || elapsed.as_nanos() * 100 >= limit.as_nanos() * 70;
            let final_pass = elapsed.as_nanos() * 100 >= limit.as_nanos() * 85;
            if pressure || ctx.round == 1 {
                let action = if final_pass {
                    "FINAL landing window: report_finding for every confirmed issue NOW, then close. Do not begin new exploration."
                } else if pressure {
                    "Stop broad exploration. Use report_finding for confirmed issues now and conclude; only inspect evidence needed to settle a specific suspicion."
                } else {
                    "Prioritize the changed code and use report_finding as soon as an issue is confirmed. Budget expires even if round budget remains; report partial coverage honestly."
                };
                messages.push(Message::synthetic_user(format!(
                    "[review time budget] {remaining}s remaining. {action}"
                )));
            }
        }
        // No round budget → time pressure above still applies.
        let Some(max) = ctx.max_rounds else { return };
        if max == 0 {
            return;
        }
        let round = ctx.round;
        // `pre_request` only runs for round in 1..=max (the fuse returns before it at
        // round > max), so `round >= max` uniquely identifies the last executing round.
        // A one-line stderr trace (same channel as the CLI's `[rules]`/`[scope]` lines)
        // makes the pressure observable — you can see exactly when landing was forced.
        // Only when the caller owns the terminal (`tracing_to_stderr`).
        if round >= max {
            if self.trace {
                eprintln!("[budget] round {round}/{max}: injected FINAL landing reminder");
            }
            messages.push(Message::synthetic_user(final_reminder(round, max)));
        } else if round as f32 >= max as f32 * self.threshold {
            if self.trace {
                eprintln!("[budget] round {round}/{max}: injected soft landing reminder");
            }
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn wall_clock_pressure_lands_even_with_many_rounds_remaining() {
        let hook = RoundBudgetHook::new().with_duration(Some(std::time::Duration::from_secs(100)));
        tokio::time::advance(std::time::Duration::from_secs(70)).await;
        let mut messages = vec![];
        hook.pre_request(&mut messages, &ctx(3, Some(200))).await;
        assert!(messages
            .iter()
            .any(|m| m.text.contains("report_finding") && m.text.contains("30s")));
        tokio::time::advance(std::time::Duration::from_secs(15)).await;
        let mut messages = vec![];
        hook.pre_request(&mut messages, &ctx(4, None)).await;
        assert!(messages
            .iter()
            .any(|m| m.text.contains("FINAL") && m.text.contains("15s")));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn default_long_budget_starts_soft_landing_at_five_minutes() {
        let hook = RoundBudgetHook::new().with_duration(Some(std::time::Duration::from_secs(900)));
        tokio::time::advance(std::time::Duration::from_secs(299)).await;
        let mut messages = vec![];
        hook.pre_request(&mut messages, &ctx(2, Some(200))).await;
        assert!(messages.is_empty());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        hook.pre_request(&mut messages, &ctx(3, Some(200))).await;
        assert!(messages
            .iter()
            .any(|m| m.text.contains("Stop broad exploration") && m.text.contains("600s")));
        assert!(messages.iter().all(|m| !m.text.contains("FINAL")));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn duration_only_budget_is_visible_before_exploration() {
        let hook = RoundBudgetHook::new().with_duration(Some(std::time::Duration::from_secs(300)));
        let mut messages = vec![];
        hook.pre_request(&mut messages, &ctx(1, None)).await;
        assert!(messages
            .iter()
            .any(|m| m.text.contains("300s") && m.text.contains("report_finding")));
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
