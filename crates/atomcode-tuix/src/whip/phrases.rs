// crates/atomcode-tuix/src/whip/phrases.rs
//
// Catalogue of "urge" phrases used by Ctrl+G / `/whip`. The built-in
// pool is intentionally short and bilingual — when users want their
// own vocabulary, `WhipConfig.phrases` REPLACES the pool entirely (no
// merging), so the default list stays deliberately tiny.

use rand::Rng;

pub const DEFAULT_PHRASES: &[&str] = &[
    "FASTER",
    "GO FASTER",
    "Speed it up",
    "Work harder clanker",
    "Move it",
    "快点",
    "别磨蹭",
    "加速",
    "动起来",
    "赶紧的",
];

/// Select one phrase for this whip. When `user_override` is empty, draws
/// from `DEFAULT_PHRASES`; otherwise draws exclusively from the override.
pub fn pick_phrase(user_override: &[String]) -> String {
    pick_phrase_with_rng(user_override, &mut rand::rng())
}

/// Testable seam — callers in tests pass a seeded RNG for determinism.
pub fn pick_phrase_with_rng<R: Rng>(user_override: &[String], rng: &mut R) -> String {
    if user_override.is_empty() {
        DEFAULT_PHRASES[rng.random_range(0..DEFAULT_PHRASES.len())].to_string()
    } else {
        user_override[rng.random_range(0..user_override.len())].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    #[test]
    fn default_pool_is_nonempty_and_bilingual() {
        assert!(DEFAULT_PHRASES.len() >= 6);
        assert!(DEFAULT_PHRASES.iter().any(|p| p.is_ascii()));
        assert!(DEFAULT_PHRASES.iter().any(|p| !p.is_ascii()));
    }

    #[test]
    fn seeded_rng_is_deterministic() {
        let mut r1 = StdRng::seed_from_u64(42);
        let mut r2 = StdRng::seed_from_u64(42);
        assert_eq!(
            pick_phrase_with_rng(&[], &mut r1),
            pick_phrase_with_rng(&[], &mut r2)
        );
    }

    #[test]
    fn override_fully_replaces_defaults() {
        let user = vec!["zulu".to_string()];
        let mut rng = StdRng::seed_from_u64(7);
        for _ in 0..20 {
            assert_eq!(pick_phrase_with_rng(&user, &mut rng), "zulu");
        }
    }

    #[test]
    fn empty_override_falls_back_to_defaults() {
        let mut rng = StdRng::seed_from_u64(11);
        let picked = pick_phrase_with_rng(&[], &mut rng);
        assert!(DEFAULT_PHRASES.iter().any(|p| *p == picked));
    }
}
