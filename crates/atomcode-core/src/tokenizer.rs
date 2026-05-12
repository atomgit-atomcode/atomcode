//! Precise token counting via the model server's own `/tokenize`
//! endpoint, with a learned-ratio cache so we only spend the RTT
//! when it actually matters.
//!
//! Mirrors Claude Code's `tokenEstimation.ts` design:
//!   1. Default path: keep the `len/4` rough estimator everywhere
//!      (32 call sites; sync; zero RTT).
//!   2. Decision points (e.g. just before sending a request that
//!      may overflow `max_model_len`): call the server's
//!      `/tokenize` endpoint to get the *true* count, then act on
//!      the truth.
//!   3. After every API count we record the
//!      `real / rough` ratio per model. Subsequent rough estimates
//!      get multiplied by the cached ratio (capped 1.0..2.0) so
//!      the rough path itself drifts toward truth without any
//!      extra RTT.
//!
//! Why not embed a tokenizer? See the docs/precise-token-count
//! analysis from 5/10: shipping per-model tokenizer.json (≈19 MB
//! each for GLM, ≈7 MB for Qwen/DeepSeek, etc.) bloats the binary
//! by 12-50 MB, requires per-model maintenance, and is *less*
//! accurate than asking the server (server uses the exact same
//! chat-template + tokenizer that `--max-model-len` is enforced
//! against). The server `/tokenize` round-trip is ~120 ms once
//! per session; the rough path stays sub-millisecond.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

/// `model_name → (real_tokens / rough_estimate) ratio` learned
/// from the most recent `/tokenize` round-trip. Updated whenever
/// `record_observation` is called; read by `apply_learned_ratio`
/// to nudge sync rough counts toward the server's truth.
///
/// Keyed by *upstream* model name (not litellm alias) so that a
/// `glm-5.1` deployment and a hypothetical `glm-5.1-pro`
/// deployment can independently learn different ratios if the
/// chat-template diverges.
static RATIO_CACHE: OnceLock<RwLock<HashMap<String, f32>>> = OnceLock::new();

/// Min/max clamps for the learned ratio. Prevents a single
/// outlier (e.g. a tiny request where the rough estimate
/// is 4 and real is 8 → ratio 2.0) from turning *every*
/// future request into a panic-level overestimate.
const RATIO_MIN: f32 = 1.00;
const RATIO_MAX: f32 = 2.00;

/// When apply_learned_ratio() has no cached ratio yet, what
/// should we assume? 1.30 ≈ the GLM-5.1 baseline observed on
/// 5/10 (atomcode estimated 49 128 → server reported 65 537
/// before cache_control_injection cleanup; ≈ 59 605 after).
/// Conservative-but-not-paranoid default.
const RATIO_DEFAULT: f32 = 1.30;

fn cache() -> &'static RwLock<HashMap<String, f32>> {
    RATIO_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Bump the per-model learned ratio after a fresh `/tokenize`
/// observation. Smoothed with a 50/50 EWMA so a single noisy
/// sample can't yank the cache off a stable estimate.
pub fn record_observation(model: &str, rough: usize, real: usize) {
    if rough == 0 || real == 0 {
        return;
    }
    let raw = (real as f32 / rough as f32).clamp(RATIO_MIN, RATIO_MAX);
    let mut map = cache().write().unwrap();
    let prev = map.get(model).copied().unwrap_or(raw);
    let smoothed = (prev + raw) / 2.0;
    map.insert(model.to_string(), smoothed.clamp(RATIO_MIN, RATIO_MAX));
}

/// Apply the learned ratio to a sync rough estimate. Used by
/// budget-overflow checks that can't await the server but want a
/// less-wrong number than `len/4` straight up.
///
/// Returns `rough` unchanged if no ratio has ever been recorded
/// for this model — the very first request of a session sees the
/// `RATIO_DEFAULT` instead so we still err safely-conservative.
pub fn apply_learned_ratio(model: &str, rough: usize) -> usize {
    let ratio = cache()
        .read()
        .unwrap()
        .get(model)
        .copied()
        .unwrap_or(RATIO_DEFAULT);
    ((rough as f32) * ratio).round() as usize
}

/// Whether we've ever recorded an observation for this model.
/// `false` → caller should consider proactively calling
/// `/tokenize` once to seed the cache before relying on
/// `apply_learned_ratio`.
pub fn has_observation(model: &str) -> bool {
    cache().read().unwrap().contains_key(model)
}

/// Test/debug helper: clear all learned ratios. Not exposed in
/// release-only paths; tests use it to start from a known state.
#[cfg(test)]
pub(crate) fn reset_for_test() {
    cache().write().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First request of a session — no observation yet — gets the
    /// conservative default ratio applied. Important: that's
    /// safer than returning the raw rough (which would be too
    /// low) because budget checks should err on the side of
    /// "compress earlier" not "blow past max_model_len".
    #[test]
    fn fresh_cache_uses_default_ratio() {
        reset_for_test();
        let bumped = apply_learned_ratio("glm-5.1", 1000);
        assert_eq!(bumped, 1300, "default 1.30x should bump 1000 → 1300");
    }

    /// After observing a 1.50x real-vs-rough sample, a *second*
    /// observation at 1.30x should land halfway (EWMA 50/50)
    /// at 1.40 — neither sample dominates the cache.
    #[test]
    fn ewma_smoothing_prevents_outlier_dominance() {
        reset_for_test();
        // First sample: rough 100, real 150 → ratio 1.50
        record_observation("test-model", 100, 150);
        let after_one = apply_learned_ratio("test-model", 1000);
        assert_eq!(after_one, 1500);
        // Second sample: rough 100, real 130 → ratio 1.30
        // Smoothed: (1.50 + 1.30) / 2 = 1.40
        record_observation("test-model", 100, 130);
        let after_two = apply_learned_ratio("test-model", 1000);
        assert_eq!(after_two, 1400);
    }

    /// Adversarial input: a tiny request where rough=4 real=8
    /// would naively yield ratio=2.0. Clamped to RATIO_MAX so
    /// the cache stays usable. Combined with smoothing, an
    /// outlier converges out within ~3 samples.
    #[test]
    fn ratio_clamped_to_safe_range() {
        reset_for_test();
        record_observation("test-model", 1, 100); // would be 100x
        let bumped = apply_learned_ratio("test-model", 1000);
        // First sample is ratio = clamp(100, 1.0, 2.0) = 2.0,
        // but smoothed against the default seed (no prior). The
        // smoothing branch only kicks in when there *is* a
        // prior, so the first observation lands at 2.0 directly.
        // This still beats 100x: budget overflow protection > false
        // positive on a tiny request.
        assert_eq!(bumped, 2000);
    }

    /// Zero inputs are no-ops — guards against calling sites that
    /// might pass empty messages without checking.
    #[test]
    fn zero_inputs_skip_cache_update() {
        reset_for_test();
        record_observation("test-model", 0, 100);
        record_observation("test-model", 100, 0);
        assert!(!has_observation("test-model"));
    }

    /// `has_observation` must return false on a clean cache so
    /// callers know to seed via /tokenize before trusting
    /// apply_learned_ratio's default-ratio output for critical
    /// budget decisions.
    #[test]
    fn fresh_cache_reports_no_observation() {
        reset_for_test();
        assert!(!has_observation("never-seen"));
        record_observation("seen", 100, 130);
        assert!(has_observation("seen"));
        assert!(!has_observation("never-seen"));
    }
}
