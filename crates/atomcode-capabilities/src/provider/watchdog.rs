//! Content progress deadlines shared by provider transports and their host.
use atomcode_kernel::stream::StreamEvent;
use std::time::Duration;
use tokio::time::Instant;

/// A positive-integer-seconds duration read from env var `var`: `None` when unset,
/// non-numeric, or ≤ 0 (so a bogus/zero value falls back to the caller's default
/// rather than silently disabling the timeout).
fn env_duration_secs(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
}

/// The default content-idle stream timeout: `ATOMCODE_STREAM_TIMEOUT_SECS` if set
/// to a valid positive integer, else 300s. Reasoning and tool argument progress
/// reset this budget; a slow stream must not be mistaken for a dead connection.
///
/// One source for every agent the product runs: the conversation, and the
/// `code_review` child reviewer, which assembles its own config and used to carry
/// a private 120s that cut long-reasoning models off well before the main loop would.
pub fn default_stream_timeout() -> Duration {
    env_duration_secs("ATOMCODE_STREAM_TIMEOUT_SECS").unwrap_or_else(|| Duration::from_secs(300))
}

/// The default first-token (prefill / TTFB) timeout. An explicit
/// `ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS` (valid positive integer) wins as-is — a
/// deliberate user choice, even if shorter than `stream_timeout`. Otherwise it
/// defaults to 600s but is NEVER shorter than `stream_timeout`: prefill on a slow
/// local model legitimately exceeds inter-token latency, so a first-token budget
/// below the inter-token one inverts the intent. In particular a user who raised
/// `ATOMCODE_STREAM_TIMEOUT_SECS` (e.g. following the reconnect hint) must not end
/// up with a SHORTER prefill window than inter-token — hence the `.max()`.
pub fn default_first_token_timeout() -> Duration {
    env_duration_secs("ATOMCODE_FIRST_TOKEN_TIMEOUT_SECS")
        .unwrap_or_else(|| default_stream_timeout().max(Duration::from_secs(600)))
}

/// A first-content deadline followed by an idle deadline. Metadata never resets it.
pub struct StreamWatchdog {
    first: Option<Duration>,
    idle: Option<Duration>,
    started: Instant,
    last_content: Option<Instant>,
}

impl StreamWatchdog {
    pub fn new(first: Option<Duration>, idle: Option<Duration>) -> Self {
        Self {
            first,
            idle,
            started: Instant::now(),
            last_content: None,
        }
    }
    fn content_deadline(&self) -> Option<Instant> {
        let budget = if self.last_content.is_some() {
            self.idle
        } else {
            self.first
        };
        budget.map(|d| self.last_content.unwrap_or(self.started) + d)
    }
    pub fn budget(&self) -> Option<Duration> {
        if self.last_content.is_some() {
            self.idle
        } else {
            self.first
        }
    }
    pub fn phase(&self) -> &'static str {
        if self.last_content.is_some() {
            "stream_idle"
        } else {
            "first_content"
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.content_deadline()
    }
    pub fn silent_for(&self) -> Duration {
        self.last_content.unwrap_or(self.started).elapsed()
    }
    /// Enforce the deadline even when a stream continuously yields ready metadata.
    pub async fn next<S: futures::Stream + Unpin>(
        &self,
        stream: &mut S,
    ) -> Result<Option<S::Item>, tokio::time::error::Elapsed> {
        use futures::StreamExt;
        let Some(deadline) = self.deadline() else {
            return Ok(stream.next().await);
        };
        if Instant::now() >= deadline {
            return tokio::time::timeout_at(deadline, futures::future::pending()).await;
        }
        tokio::time::timeout_at(deadline, stream.next()).await
    }

    pub fn observe(&mut self, event: &StreamEvent) {
        let content = match event {
            StreamEvent::TextDelta(text) | StreamEvent::Reasoning(text) => !text.is_empty(),
            StreamEvent::ReasoningSignature { opaque, .. } => !opaque.is_empty(),
            StreamEvent::ToolCall(_) => true,
            StreamEvent::ToolCallDelta {
                id,
                name,
                arguments,
                ..
            } => {
                !arguments.is_empty()
                    || id.as_ref().is_some_and(|s| !s.is_empty())
                    || name.as_ref().is_some_and(|s| !s.is_empty())
            }
            _ => false,
        };
        if content {
            self.last_content = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_slow_but_progressing_stream_can_outlive_both_timeout_budgets() {
        let mut watchdog = StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(300)),
        );
        let started = Instant::now();
        for _ in 0..5 {
            let chunk = futures::stream::once(async {
                tokio::time::sleep(Duration::from_secs(240)).await;
                StreamEvent::TextDelta("more".into())
            });
            futures::pin_mut!(chunk);
            let event = watchdog
                .next(&mut chunk)
                .await
                .expect("progress within the idle window")
                .expect("chunk");
            watchdog.observe(&event);
        }
        assert_eq!(started.elapsed(), Duration::from_secs(1200));
        assert_eq!(
            watchdog.deadline().unwrap() - Instant::now(),
            Duration::from_secs(300)
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn expired_deadlines_cannot_be_bypassed_by_ready_events() {
        let w = StreamWatchdog::new(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)));
        tokio::time::advance(Duration::from_secs(2)).await;
        let mut stream = futures::stream::repeat(StreamEvent::Usage(Default::default()));
        assert!(w.next(&mut stream).await.is_err());
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn metadata_does_not_cut_short_or_extend_the_first_content_deadline() {
        let mut w = StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(120)),
        );
        let first = w.deadline().unwrap();
        tokio::time::advance(Duration::from_secs(150)).await;
        w.observe(&StreamEvent::TextDelta(String::new()));
        w.observe(&StreamEvent::Usage(Default::default()));
        assert_eq!(w.deadline(), Some(first));
        assert!(Instant::now() < first);
        w.observe(&StreamEvent::Reasoning("thinking".into()));
        let idle = w.deadline().unwrap();
        assert_eq!(idle - Instant::now(), Duration::from_secs(120));
        tokio::time::advance(Duration::from_secs(60)).await;
        w.observe(&StreamEvent::Usage(Default::default()));
        assert_eq!(w.deadline(), Some(idle));
        w.observe(&StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: "{".into(),
        });
        assert_eq!(
            w.deadline().unwrap() - Instant::now(),
            Duration::from_secs(120)
        );
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn each_request_gets_its_own_deadline_and_disabled_budgets_stay_disabled() {
        let w = StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(120)),
        );
        tokio::time::advance(Duration::from_secs(600)).await;
        assert!(w.deadline().unwrap() <= Instant::now());
        let mut next = StreamWatchdog::new(None, None);
        assert_eq!(next.deadline(), None);
        next.observe(&StreamEvent::TextDelta("answer".into()));
        assert_eq!(next.deadline(), None);
    }
}
