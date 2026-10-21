//! Metadata-only timings: no bodies, credentials, URLs or error messages.

use atomcode_kernel::stream::{ProviderError, StreamEvent};
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
    malformed_events: u64,
    response_id: Option<String>,
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
            malformed_events: 0,
            response_id: None,
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
            let elapsed_ms = self.started.elapsed().as_millis() as u64;
            if matches!(phase, "response_headers" | "first_reasoning" | "first_text") {
                tracing::info!(
                    request_id = self.id,
                    phase,
                    elapsed_ms,
                    "provider request timing"
                );
            } else {
                tracing::debug!(
                    request_id = self.id,
                    phase,
                    elapsed_ms,
                    "provider request timing"
                );
            }
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
            StreamEvent::ResponseId(id) => {
                self.response_id = correlation_id(id).map(str::to_owned);
            }
            StreamEvent::Malformed => {
                self.malformed_events += 1;
                if self.malformed_events == 1 {
                    self.span.in_scope(|| {
                        tracing::warn!(
                            error_origin = "malformed_event",
                            "provider stream diagnostic"
                        )
                    });
                }
            }
            StreamEvent::Done { truncated } => {
                self.span
                    .in_scope(|| tracing::info!(truncated, "provider stream terminal"));
                self.outcome = "done";
                self.mark("done");
            }
            StreamEvent::Error(error) => {
                self.log_error(error);
                self.outcome = "stream_error";
                self.mark("stream_error");
            }
            _ => {}
        }
    }
    pub fn open_failed(&mut self, error: &ProviderError) {
        self.outcome = "open_error";
        self.log_error(error);
    }

    fn log_error(&self, error: &ProviderError) {
        self.span.in_scope(|| {
            tracing::warn!(
                error_class = error_class(error),
                http_status = error.http_status,
                retryable = error.retryable,
                retry_after_secs = error.retry_after_secs,
                has_error_code = error.code.is_some(),
                had_content = self.first_reasoning || self.first_text,
                upstream_response_id = self.response_id.as_deref(),
                "provider request error metadata"
            )
        });
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
                malformed_events = self.malformed_events,
                upstream_response_id = self.response_id.as_deref(),
                "provider request timing"
            )
        });
    }
}

// Never accept whitespace, control characters, URLs or unbounded header data.
// Opaque IDs are used only to correlate with the upstream service's own logs.
pub(super) fn correlation_id(value: &str) -> Option<&str> {
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')))
    .then_some(value)
}

fn error_class(error: &ProviderError) -> &'static str {
    use atomcode_kernel::stream::{
        OPEN_TIMEOUT_CODE, STREAM_IDLE_TIMEOUT_CODE, STREAM_TRANSPORT_INTERRUPTED_CODE,
    };
    match error.code.as_deref() {
        Some(OPEN_TIMEOUT_CODE) => "open_timeout",
        Some(STREAM_IDLE_TIMEOUT_CODE) => "stream_idle_timeout",
        Some(STREAM_TRANSPORT_INTERRUPTED_CODE) => "stream_transport_interrupted",
        Some("upstream_error_body") => "upstream_error_body",
        Some("not_an_event_stream") => "not_an_event_stream",
        // Upstream codes and messages may contain sensitive data. Keep their
        // presence/status but never print arbitrary strings to the log.
        _ => "unclassified",
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
        abandoned.open_failed(&ProviderError::default());
        assert_eq!(abandoned.outcome, "open_error");
    }
    #[test]
    fn diagnostics_keep_terminal_metadata_without_payloads_or_untrusted_error_strings() {
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<std::sync::Mutex<String>>);
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                struct Visitor<'a>(&'a mut String);
                impl tracing::field::Visit for Visitor<'_> {
                    fn record_debug(
                        &mut self,
                        field: &tracing::field::Field,
                        value: &dyn std::fmt::Debug,
                    ) {
                        use std::fmt::Write;
                        let _ = write!(self.0, "{}={:?} ", field.name(), value);
                    }
                }
                event.record(&mut Visitor(&mut self.0.lock().unwrap()));
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let capture = Capture::default();
        tracing::subscriber::with_default(capture.clone(), || {
            let mut timing = Latency::new();
            timing.observe(&StreamEvent::ResponseId("chatcmpl-safe-123".into()));
            timing.observe(&StreamEvent::TextDelta("PRIVATE_CONTENT".into()));
            timing.observe(&StreamEvent::Malformed);
            timing.observe(&StreamEvent::Error(ProviderError {
                message: "PRIVATE_ERROR_BODY".into(),
                code: Some("PRIVATE_UPSTREAM_CODE".into()),
                http_status: Some(503),
                retryable: true,
                retry_after_secs: Some(7),
            }));
            assert_eq!(timing.outcome, "stream_error");
            assert_eq!(timing.malformed_events, 1);
        });
        let log = capture.0.lock().unwrap();
        for expected in [
            "http_status=503",
            "retryable=true",
            "had_content=true",
            "chatcmpl-safe-123",
            "malformed_events=1",
            "unclassified",
            "stream_error",
        ] {
            assert!(log.contains(expected), "missing {expected}: {log}");
        }
        for secret in [
            "PRIVATE_CONTENT",
            "PRIVATE_ERROR_BODY",
            "PRIVATE_UPSTREAM_CODE",
        ] {
            assert!(!log.contains(secret), "{log}");
        }
    }

    #[test]
    fn correlation_ids_reject_header_injection_urls_and_unbounded_values() {
        assert_eq!(correlation_id("req-123_abc.us:2"), Some("req-123_abc.us:2"));
        for invalid in [
            "",
            "x\nAuthorization: key",
            "https://host/?token=secret",
            "Bearer secret",
            &"x".repeat(129),
        ] {
            assert_eq!(correlation_id(invalid), None);
        }
    }
}
