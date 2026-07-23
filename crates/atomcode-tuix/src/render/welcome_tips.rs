//! Curated, usage-informed pool of "getting started" tips for the welcome banner.
//! `/login` is always pinned first; 3 more are chosen at random from `POOL`.
//! The pool is a hand-edited const, refreshed per release from the usage dashboard.

use crate::i18n::Msg;
use rand::seq::SliceRandom;
use rand::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tip {
    pub cmd: &'static str,
    pub desc: Msg<'static>,
}

/// Always shown first.
pub const PINNED: Tip = Tip {
    cmd: "/login",
    desc: Msg::WelcomeTipLogin,
};

/// Random pool (15). Filtered to onboarding-relevant commands; excludes
/// exit/clear/destructive and pure-utility commands. Edit + recompile to refresh.
pub const POOL: &[Tip] = &[
    Tip {
        cmd: "/provider",
        desc: Msg::WelcomeTipProvider,
    },
    Tip {
        cmd: "/model",
        desc: Msg::WelcomeTipModel,
    },
    Tip {
        cmd: "/resume",
        desc: Msg::WelcomeTipResume,
    },
    Tip {
        cmd: "/setup",
        desc: Msg::WelcomeTipSetup,
    },
    Tip {
        cmd: "/skills",
        desc: Msg::WelcomeTipSkills,
    },
    Tip {
        cmd: "/plugin",
        desc: Msg::WelcomeTipPlugin,
    },
    Tip {
        cmd: "/webui",
        desc: Msg::WelcomeTipWebui,
    },
    Tip {
        cmd: "/mcp",
        desc: Msg::WelcomeTipMcp,
    },
    Tip {
        cmd: "/plan",
        desc: Msg::WelcomeTipPlan,
    },
    Tip {
        cmd: "/session",
        desc: Msg::WelcomeTipSession,
    },
    Tip {
        cmd: "/loop",
        desc: Msg::WelcomeTipLoop,
    },
    Tip {
        cmd: "/goal",
        desc: Msg::WelcomeTipGoal,
    },
    Tip {
        cmd: "/init",
        desc: Msg::WelcomeTipInit,
    },
    Tip {
        cmd: "/language",
        desc: Msg::WelcomeTipLanguage,
    },
    Tip {
        cmd: "/usage",
        desc: Msg::WelcomeTipUsage,
    },
];

/// How many random tips to show below the pinned one.
const RANDOM_COUNT: usize = 3;

/// Pick `RANDOM_COUNT` distinct POOL indices (for caching a stable selection).
pub fn choose_pool_indices(rng: &mut impl Rng) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..POOL.len()).collect();
    idx.shuffle(rng);
    idx.truncate(RANDOM_COUNT.min(POOL.len()));
    idx
}

/// Resolve cached indices back to `[PINNED, ...selected]`.
pub fn tips_from_indices(indices: &[usize]) -> Vec<Tip> {
    let mut out = Vec::with_capacity(1 + indices.len());
    out.push(PINNED);
    for &i in indices {
        if let Some(t) = POOL.get(i) {
            out.push(*t);
        }
    }
    out
}

/// `[PINNED, r1, r2, r3]` — pinned first, then up to 3 distinct random picks.
pub fn choose_tips(rng: &mut impl Rng) -> Vec<Tip> {
    tips_from_indices(&choose_pool_indices(rng))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    fn fixed(seed: u64) -> StdRng {
        StdRng::seed_from_u64(seed)
    }

    #[test]
    fn pinned_is_first_and_login() {
        let t = choose_tips(&mut fixed(1));
        assert_eq!(t[0], PINNED);
        assert_eq!(t[0].cmd, "/login");
    }

    #[test]
    fn returns_exactly_four() {
        assert_eq!(choose_tips(&mut fixed(1)).len(), 4);
    }

    #[test]
    fn random_three_are_distinct_and_not_pinned() {
        let t = choose_tips(&mut fixed(7));
        let rest = &t[1..];
        for w in rest {
            assert_ne!(w.cmd, "/login");
        }
        for i in 0..rest.len() {
            for j in (i + 1)..rest.len() {
                assert_ne!(rest[i].cmd, rest[j].cmd, "duplicate random tip");
            }
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let a: Vec<_> = choose_tips(&mut fixed(42)).iter().map(|t| t.cmd).collect();
        let b: Vec<_> = choose_tips(&mut fixed(42)).iter().map(|t| t.cmd).collect();
        assert_eq!(a, b);
    }

    #[test]
    fn pool_excludes_filtered_commands() {
        let banned = [
            "/quit",
            "/clear",
            "/status",
            "/cd",
            "/logout",
            "/delete_session",
            "/undo",
            "/stop",
            "/whoami",
            "/cost",
        ];
        for t in POOL {
            assert!(!banned.contains(&t.cmd), "{} must not be in POOL", t.cmd);
        }
    }
}
