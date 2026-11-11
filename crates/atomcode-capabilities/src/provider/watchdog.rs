//! Content progress deadlines shared by provider transports and their host.
use atomcode_kernel::stream::StreamEvent;
use std::time::Duration;
use tokio::time::Instant;

/// A first-content deadline followed by an idle deadline. Metadata never resets it.
pub struct StreamWatchdog {
    first: Option<Duration>,
    idle: Option<Duration>,
    started: Instant,
    last_content: Option<Instant>,
    reasoning_limit: Option<Duration>,
    first_reasoning: Option<Instant>,
    visible_progress: bool,
}

impl StreamWatchdog {
    pub fn new(first: Option<Duration>, idle: Option<Duration>) -> Self {
        Self {
            first,
            idle,
            started: Instant::now(),
            last_content: None,
            reasoning_limit: None,
            first_reasoning: None,
            visible_progress: false,
        }
    }
    /// Bound continuous reasoning before the first answer/tool content. Prefill
    /// does not count, and useful output permanently disarms it for this request.
    pub fn with_reasoning_limit(mut self, limit: Option<Duration>) -> Self {
        self.reasoning_limit = limit.filter(|d| !d.is_zero());
        self
    }
    fn content_deadline(&self) -> Option<Instant> {
        let budget = if self.last_content.is_some() {
            self.idle
        } else {
            self.first
        };
        budget.map(|d| self.last_content.unwrap_or(self.started) + d)
    }
    fn reasoning_deadline(&self) -> Option<Instant> {
        if self.visible_progress {
            return None;
        }
        Some(self.first_reasoning? + self.reasoning_limit?)
    }
    pub fn reasoning_expired(&self) -> bool {
        self.reasoning_deadline()
            .is_some_and(|d| Instant::now() >= d)
    }
    fn reasoning_is_next(&self) -> bool {
        self.reasoning_deadline()
            .is_some_and(|r| self.content_deadline().map_or(true, |c| r <= c))
    }
    pub fn budget(&self) -> Option<Duration> {
        if self.reasoning_is_next() {
            return self.reasoning_limit;
        }
        if self.last_content.is_some() {
            self.idle
        } else {
            self.first
        }
    }
    pub fn phase(&self) -> &'static str {
        if self.reasoning_is_next() {
            return "reasoning_without_output";
        }
        if self.last_content.is_some() {
            "stream_idle"
        } else {
            "first_content"
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        match (self.content_deadline(), self.reasoning_deadline()) {
            (Some(c), Some(r)) => Some(c.min(r)),
            (c, r) => c.or(r),
        }
    }
    pub fn silent_for(&self) -> Duration {
        if self.reasoning_is_next() {
            self.first_reasoning.unwrap().elapsed()
        } else {
            self.last_content.unwrap_or(self.started).elapsed()
        }
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
        match event {
            StreamEvent::Reasoning(text) if !text.is_empty() => {
                self.first_reasoning.get_or_insert_with(Instant::now);
            }
            StreamEvent::ReasoningSignature { opaque, .. } if !opaque.is_empty() => {
                self.first_reasoning.get_or_insert_with(Instant::now);
            }
            StreamEvent::TextDelta(text) if !text.trim().is_empty() => self.visible_progress = true,
            StreamEvent::ToolCall(_) => self.visible_progress = true,
            StreamEvent::ToolCallDelta {
                id,
                name,
                arguments,
                ..
            } if !arguments.is_empty()
                || id.as_ref().is_some_and(|s| !s.is_empty())
                || name.as_ref().is_some_and(|s| !s.is_empty()) =>
            {
                self.visible_progress = true
            }
            _ => {}
        }
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
    async fn reasoning_deadline_excludes_prefill_and_cannot_be_extended_by_progress() {
        let mut w = StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(300)),
        )
        .with_reasoning_limit(Some(Duration::from_secs(120)));
        tokio::time::advance(Duration::from_secs(500)).await;
        assert!(!w.reasoning_expired());
        w.observe(&StreamEvent::Reasoning("first".into()));
        let deadline = w.deadline();
        for i in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            w.observe(&StreamEvent::Reasoning(format!("new reasoning {i}")));
            w.observe(&StreamEvent::TextDelta(" \n".into()));
            assert_eq!(w.deadline(), deadline);
        }
        assert!(w.reasoning_expired());
        assert_eq!(w.phase(), "reasoning_without_output");
        let mut ready = futures::stream::repeat(StreamEvent::Reasoning("more".into()));
        assert!(w.next(&mut ready).await.is_err());
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn real_text_or_tool_progress_disarms_the_reasoning_deadline() {
        for event in [
            StreamEvent::TextDelta("answer".into()),
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: Some("read_file".into()),
                arguments: String::new(),
            },
        ] {
            let mut w = StreamWatchdog::new(None, Some(Duration::from_secs(300)))
                .with_reasoning_limit(Some(Duration::from_secs(120)));
            w.observe(&StreamEvent::Reasoning("first".into()));
            tokio::time::advance(Duration::from_secs(60)).await;
            w.observe(&event);
            tokio::time::advance(Duration::from_secs(120)).await;
            assert!(!w.reasoning_expired());
            assert_eq!(w.phase(), "stream_idle");
            assert!(w.deadline().unwrap() > Instant::now());
        }
        let mut disabled =
            StreamWatchdog::new(None, None).with_reasoning_limit(Some(Duration::ZERO));
        disabled.observe(&StreamEvent::Reasoning("first".into()));
        tokio::time::advance(Duration::from_secs(1000)).await;
        assert_eq!(disabled.deadline(), None);
    }
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
