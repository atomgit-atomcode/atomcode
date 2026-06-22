//! Turn-synthesis helpers shared by the native drivers (cli-headless, daemon, tuix).
//!
//! The kernel event stream is deliberately lean: a `ToolResult` carries only its
//! `call_id` (no tool name, no duration), and `TurnComplete` carries only a
//! `StopReason` (no token / round / tool tallies). Every native driver therefore has
//! to re-synthesize that metadata locally. These two small stateful helpers hold the
//! synthesis that was previously copy-pasted per driver (and originated in the bridge).
//!
//! Pure data — no kernel / core dependency — so each driver keeps its own event→output
//! mapping and just feeds these helpers as it walks the stream.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Recovers the tool name + elapsed time that a kernel `ToolResult` drops. A driver
/// `record`s each `ToolStarted` and `resolve`s on the matching `ToolResult`.
#[derive(Default)]
pub struct LiveTools {
    inflight: HashMap<String, (String, Instant)>,
}

impl LiveTools {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember an in-flight call so its result can recover the name + start time.
    pub fn record(&mut self, call_id: &str, name: &str) {
        self.inflight
            .insert(call_id.to_string(), (name.to_string(), Instant::now()));
    }

    /// Read back `(name, elapsed)` for a finished call. Falls back to `("tool", ZERO)`
    /// when the start was never recorded (out-of-order / dropped start) — never panics.
    pub fn resolve(&mut self, call_id: &str) -> (String, Duration) {
        self.inflight
            .remove(call_id)
            .map(|(name, started)| (name, started.elapsed()))
            .unwrap_or_else(|| ("tool".to_string(), Duration::ZERO))
    }
}

/// Per-turn accumulators backing a synthesized turn summary. The kernel `TurnComplete`
/// carries only a `StopReason`, so duration / rounds / tool-calls / tokens are tallied here.
#[derive(Default)]
pub struct TurnStats {
    started: Option<Instant>,
    rounds: usize,
    tool_calls: usize,
    total_tokens: usize,
}

impl TurnStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stamp the turn start (for `duration`).
    pub fn start(&mut self) {
        self.started = Some(Instant::now());
    }

    /// Count one tool invocation (`ToolStarted`).
    pub fn on_tool_started(&mut self) {
        self.tool_calls += 1;
    }

    /// Count one provider round and its tokens (`Usage`). Each `Usage` event == one
    /// round, matching the headless / bridge tally.
    pub fn on_usage(&mut self, prompt: usize, completion: usize) {
        self.rounds += 1;
        self.total_tokens += prompt + completion;
    }

    pub fn rounds(&self) -> usize {
        self.rounds
    }

    pub fn tool_calls(&self) -> usize {
        self.tool_calls
    }

    pub fn total_tokens(&self) -> usize {
        self.total_tokens
    }

    /// Elapsed since `start` (ZERO if `start` was never called).
    pub fn duration(&self) -> Duration {
        self.started.map(|s| s.elapsed()).unwrap_or(Duration::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_tools_recovers_name_and_duration() {
        let mut live = LiveTools::new();
        live.record("call-1", "bash");
        let (name, dur) = live.resolve("call-1");
        assert_eq!(name, "bash", "name recovered (kernel result carries none)");
        assert!(dur < Duration::from_secs(1), "duration is the short elapsed");
        // Resolving the same id again is now unknown → safe fallback, no panic.
        let (name2, dur2) = live.resolve("call-1");
        assert_eq!(name2, "tool", "consumed call_id falls back to \"tool\"");
        assert_eq!(dur2, Duration::ZERO, "consumed call_id falls back to ZERO");
    }

    #[test]
    fn live_tools_unknown_call_id_is_safe() {
        let mut live = LiveTools::new();
        let (name, dur) = live.resolve("never-recorded");
        assert_eq!(name, "tool");
        assert_eq!(dur, Duration::ZERO);
    }

    #[test]
    fn turn_stats_tallies_rounds_tokens_and_tools() {
        let mut stats = TurnStats::new();
        stats.start();
        stats.on_tool_started();
        stats.on_tool_started();
        stats.on_usage(10, 5);
        stats.on_usage(3, 2);
        assert_eq!(stats.tool_calls(), 2);
        assert_eq!(stats.rounds(), 2, "each Usage event counts one round");
        assert_eq!(stats.total_tokens(), 20, "prompt+completion summed across rounds");
        assert!(stats.duration() < Duration::from_secs(1));
    }

    #[test]
    fn turn_stats_duration_zero_without_start() {
        let stats = TurnStats::new();
        assert_eq!(stats.duration(), Duration::ZERO, "no start → ZERO, not a panic");
    }
}
