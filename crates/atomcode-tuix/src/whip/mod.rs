// crates/atomcode-tuix/src/whip/mod.rs
//
// The "whip" feature — Ctrl+G / `/whip` nudge. Split into its own module
// so the tuix event loop doesn't grow another ad-hoc feature dir:
//   - phrases.rs : phrase pool + RNG-backed picker
//   - anim.rs    : frame generator (added in Task 4)
//   - mod.rs     : Cooldown + fire_whip orchestration (fire_whip in Task 7)

pub mod anim;
pub mod phrases;

use std::time::{Duration, Instant};

/// Monotonic rate-limit gate shared by Ctrl+G and `/whip`. `last` is
/// stored on `LoopCtx` (not inside this struct) so a single source of
/// truth lives with the event loop; this struct is a stateless helper.
pub struct Cooldown;

impl Cooldown {
    /// Returns true if a whip may fire right now (the stored `last` is
    /// either None or older than `window`). Callers update `last` on
    /// their own after a successful fire.
    pub fn try_fire(last: Option<Instant>, now: Instant, window: Duration) -> bool {
        match last {
            None => true,
            Some(t) => now.duration_since(t) >= window,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fire_is_always_allowed() {
        let now = Instant::now();
        assert!(Cooldown::try_fire(None, now, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_within_window_is_blocked() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(500);
        assert!(!Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_exactly_at_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1000);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }

    #[test]
    fn fire_after_window_is_allowed() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1001);
        assert!(Cooldown::try_fire(Some(t0), t1, Duration::from_millis(1000)));
    }
}
