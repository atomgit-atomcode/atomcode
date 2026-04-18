// crates/atomcode-tuix/src/render/throttle.rs
//
// Input-prompt render throttle.
//
// Mac Terminal.app takes ~30-60ms to process a full footer ANSI payload.
// A fast typist (8 chars/sec) submits InputPrompt / StreamingBox renders
// twice as fast as Terminal.app can drain, producing the "cursor blinks
// but no chars, then they catch up in a burst" symptom. This layer
// smooths that by capping input-driven redraws to ~50fps and parking
// the most recent payload for the trailing edge.
//
// Only InputPrompt / StreamingBox are throttled — ToolCall / Welcome /
// diff / error / etc. bypass (they're low-frequency and we want
// immediacy).
//
// Orchestration contract (used by AnsiRenderer::render):
//   if InputThrottle::is_throttled(&line):
//     if window_elapsed:   paint now + mark_painted + clear_pending
//     else:                park_pending(line)         // trailing edge
//   else:
//     paint now                                      // bypass

use std::time::{Duration, Instant};

use super::UiLine;

/// Minimum gap between two InputPrompt / StreamingBox redraws.
///
/// Lowered from 20ms to 5ms after Step 9 (render worker on dedicated
/// OS thread). The old 20ms + 20ms deferred-tick added up to ~40ms
/// visible lag for IME commit bursts (e.g. macOS Pinyin "达到的地方"
/// arriving as 5 Char events in <100µs), which fast typists
/// perceived as "I pressed space, the chars didn't show, I need to
/// type again". With the render worker now absorbing terminal I/O
/// asynchronously, the throttle's only remaining job is to coalesce
/// sub-5ms bursts — normal human typing (>50ms inter-key) never hits
/// it now.
pub const INPUT_REDRAW_THROTTLE_MS: u64 = 5;

#[derive(Default)]
pub struct InputThrottle {
    last_paint: Option<Instant>,
    pending: Option<UiLine>,
}

impl InputThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Is the payload worth throttling? True only for the two
    /// high-frequency input-driven renders.
    pub fn is_throttled(line: &UiLine) -> bool {
        matches!(line, UiLine::InputPrompt { .. } | UiLine::StreamingBox { .. })
    }

    /// Has enough time elapsed since the last input-driven paint to let
    /// a new one through? Leading edge (`None`) always returns true.
    pub fn window_elapsed(&self) -> bool {
        let elapsed = match self.last_paint {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_millis(INPUT_REDRAW_THROTTLE_MS),
        };
        crate::tuix_trace!(
            "THR",
            "window_elapsed={} since_last={:?}",
            elapsed,
            self.last_paint.map(|t| t.elapsed())
        );
        elapsed
    }

    /// Record that we just painted an input-driven render. Also drops
    /// any pending payload since it's now stale.
    pub fn mark_painted(&mut self) {
        self.last_paint = Some(Instant::now());
        self.pending = None;
        crate::tuix_trace!("THR", "mark_painted");
    }

    /// Park an input-driven render for the trailing-edge paint. If one
    /// was already parked it's replaced (the latest state wins — the
    /// user doesn't want to see an intermediate buffer).
    pub fn park_pending(&mut self, line: UiLine) {
        let had_prior = self.pending.is_some();
        self.pending = Some(line);
        crate::tuix_trace!("THR", "park_pending replaced_prior={}", had_prior);
    }

    /// Take the parked payload (if any) and return it for painting.
    /// Callers that take a payload must call `mark_painted` after
    /// painting so the next window starts fresh.
    pub fn take_pending(&mut self) -> Option<UiLine> {
        self.pending.take()
    }

    /// Is something waiting to paint?
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Forget all cached state. Used by `reset()` when a child process
    /// has taken the terminal and our throttle state is no longer valid.
    pub fn clear(&mut self) {
        self.last_paint = None;
        self.pending = None;
        crate::tuix_trace!("THR", "clear");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::StatusLine;

    fn sample_input() -> UiLine {
        UiLine::InputPrompt {
            buf: "hi".into(),
            cursor_byte: 2,
            menu: None,
            status: StatusLine::default(),
        }
    }

    #[test]
    fn input_prompt_is_throttled() {
        assert!(InputThrottle::is_throttled(&sample_input()));
    }

    #[test]
    fn tool_call_is_not_throttled() {
        let tc = UiLine::ToolCall {
            name: "read".into(),
            detail: "a.rs".into(),
        };
        assert!(!InputThrottle::is_throttled(&tc));
    }

    #[test]
    fn leading_edge_window_is_open() {
        let t = InputThrottle::new();
        assert!(t.window_elapsed());
    }

    #[test]
    fn mark_painted_closes_window_and_clears_pending() {
        let mut t = InputThrottle::new();
        t.park_pending(sample_input());
        assert!(t.has_pending());
        t.mark_painted();
        // Window is closed now (we just painted).
        assert!(!t.window_elapsed());
        assert!(!t.has_pending()); // stale parked payload dropped
    }

    #[test]
    fn take_pending_drains() {
        let mut t = InputThrottle::new();
        t.park_pending(sample_input());
        assert!(t.take_pending().is_some());
        assert!(t.take_pending().is_none());
    }

    #[test]
    fn clear_resets_both_fields() {
        let mut t = InputThrottle::new();
        t.mark_painted();
        t.park_pending(sample_input());
        t.clear();
        assert!(t.window_elapsed());
        assert!(!t.has_pending());
    }
}
