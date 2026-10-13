//! Metadata-only timings: no bodies, credentials, URLs or error messages.

use atomcode_kernel::stream::StreamEvent;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT: AtomicU64 = AtomicU64::new(1);

pub(super) struct Latency {
    pub id: u64,
    started: Instant,
    span: tracing::Span,
    first_reasoning: bool,
    first_text: bool,
    reasoning_chars: usize,
    text_chars: usize,
    outcome: &'static str,
}

impl Latency {
    pub fn new() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            started: Instant::now(),
            span: tracing::info_span!(
                "provider_request",
                request_id = id,
                process_id = std::process::id()
            ),
            first_reasoning: false,
            first_text: false,
            reasoning_chars: 0,
            text_chars: 0,
            outcome: "dropped_before_terminal",
        }
    }
    pub fn started(&self) -> Instant {
        self.started
    }

    pub fn span(&self) -> tracing::Span {
        self.span.clone()
    }

    pub fn mark(&self, phase: &'static str) {
        self.span.in_scope(|| {
            tracing::info!(
                request_id = self.id,
                phase,
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                "provider request timing"
            )
        });
    }
    pub fn observe(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::Reasoning(text) if !text.is_empty() => {
                if !self.first_reasoning {
                    self.mark("first_reasoning");
                }
                self.first_reasoning = true;
                self.reasoning_chars += text.chars().count();
            }
            StreamEvent::TextDelta(text) if !text.is_empty() => {
                if !self.first_text {
                    self.mark("first_text");
                }
                self.first_text = true;
                self.text_chars += text.chars().count();
            }
            StreamEvent::Done { .. } => {
                self.outcome = "done";
                self.mark("done");
            }
            StreamEvent::Error(_) => {
                self.outcome = "stream_error";
                self.mark("stream_error");
            }
            _ => {}
        }
    }
    pub fn open_failed(&mut self) {
        self.outcome = "open_error";
    }
}

impl Drop for Latency {
    fn drop(&mut self) {
        self.span.in_scope(|| {
            tracing::info!(
                request_id = self.id,
                phase = "request_end",
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                outcome = self.outcome,
                reasoning_chars = self.reasoning_chars,
                text_chars = self.text_chars,
                "provider request timing"
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_unicode_and_preserves_terminal_outcomes() {
        let mut timing = Latency::new();
        timing.observe(&StreamEvent::TextDelta(String::new()));
        assert!(!timing.first_text);
        timing.observe(&StreamEvent::Reasoning("想想".into()));
        timing.observe(&StreamEvent::TextDelta("你好".into()));
        timing.observe(&StreamEvent::TextDelta("!".into()));
        assert_eq!((timing.reasoning_chars, timing.text_chars), (2, 3));
        timing.observe(&StreamEvent::Done { truncated: false });
        assert_eq!(timing.outcome, "done");
        let mut abandoned = Latency::new();
        assert_eq!(abandoned.outcome, "dropped_before_terminal");
        abandoned.open_failed();
        assert_eq!(abandoned.outcome, "open_error");
    }
}
