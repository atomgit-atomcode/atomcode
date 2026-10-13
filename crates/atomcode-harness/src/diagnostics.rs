//! Metadata-only phase timing; never accepts prompt text or command output.
use std::time::Instant;

/// A phase always has an end record, including when its future is dropped.
/// `returned` means control returned, not that the operation succeeded.
pub struct PhaseTiming {
    phase: &'static str,
    started: Instant,
    span: tracing::Span,
    outcome: &'static str,
}

impl PhaseTiming {
    pub fn start(phase: &'static str) -> Self {
        let timing = Self {
            phase,
            started: Instant::now(),
            span: tracing::Span::current(),
            outcome: "dropped_before_return",
        };
        timing.span.in_scope(|| {
            tracing::info!(
                phase,
                state = "start",
                process_id = std::process::id(),
                "request preparation timing"
            )
        });
        timing
    }

    pub fn returned(&mut self) {
        self.outcome = "returned";
    }
}

impl Drop for PhaseTiming {
    fn drop(&mut self) {
        self.span.in_scope(|| {
            tracing::info!(
                phase = self.phase,
                state = "end",
                process_id = std::process::id(),
                elapsed_ms = self.started.elapsed().as_millis() as u64,
                outcome = self.outcome,
                "request preparation timing"
            )
        });
    }
}
