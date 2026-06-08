//! Injectable monotonic clock — the kernel's one determinism seam for TIME.
//!
//! The kernel reads a [`Clock`] ONLY to stamp a turn's `elapsed_ms` (a `MessageMeta`
//! sidecar — never part of the wire/prefix, never affecting model behavior or the prefix
//! cache). That `elapsed_ms` is the kernel's SOLE non-deterministic value: ids are
//! monotonic counters, cwd is pinnable (`AgentBuilder::working_dir`), and there is no
//! randomness anywhere. Injecting a [`FixedClock`] therefore makes a whole run's
//! snapshots BYTE-reproducible for eval / replay (e.g. SWE-bench grading diffs).

use std::time::Instant;

/// A monotonic millisecond source. Only DIFFERENCES between two reads are meaningful
/// (the kernel measures `end - start` for one turn's elapsed time).
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> u64;
}

/// The real monotonic clock — the kernel default.
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }
}

/// A FIXED clock: `now_millis` always returns the same value, so every measured
/// `elapsed_ms` is `0` — making a run's snapshots reproducible for eval / replay.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_yields_zero_elapsed() {
        let c = FixedClock(42);
        let start = c.now_millis();
        let end = c.now_millis();
        assert_eq!(end.saturating_sub(start), 0, "a fixed clock makes elapsed always 0");
    }

    #[test]
    fn system_clock_is_monotonic_nondecreasing() {
        let c = SystemClock::new();
        let a = c.now_millis();
        let b = c.now_millis();
        assert!(b >= a, "a monotonic clock never goes backwards");
    }
}
