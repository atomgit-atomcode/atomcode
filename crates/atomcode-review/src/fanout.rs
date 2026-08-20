//! Deep-mode dimension fan-out for `code_review`: many read-only reviewers, one
//! per concern lens, merged into a single deduped finding set. See
//! docs/plans/2026-08-20-code-review-deep-mode-fanout-design.md.

/// One review lens. `lens` is appended to the base reviewer persona via
/// `ReviewAgentConfig::with_persona_append`, biasing focus without replacing the
/// shared reviewer instructions.
pub struct ReviewDimension {
    pub id: &'static str,
    pub display: &'static str,
    pub lens: &'static str,
}

/// The concern dimensions a deep review fans out across, in display order. Each
/// reviewer sees the FULL diff through its lens; overlap is resolved by
/// `merge_findings` (favor recall).
pub const REVIEW_DIMENSIONS: &[ReviewDimension] = &[
    ReviewDimension {
        id: "correctness",
        display: "Correctness",
        lens: "\n\n## This review's lens: CORRECTNESS\nConcentrate on logic errors, wrong \
               conditions, off-by-one, unhandled edge cases, error handling, concurrency/races, \
               and regressions introduced by this diff. Still report anything clearly severe you \
               notice outside this lens.",
    },
    ReviewDimension {
        id: "security",
        display: "Security",
        lens: "\n\n## This review's lens: SECURITY\nConcentrate on injection, missing authz/authn, \
               secret handling, unsafe deserialization, path/SSRF issues, and supply-chain surface \
               (dependency, CI, and config changes). Still report anything clearly severe you \
               notice outside this lens.",
    },
    ReviewDimension {
        id: "performance",
        display: "Performance",
        lens: "\n\n## This review's lens: PERFORMANCE\nConcentrate on hot-path cost, needless \
               allocations/clones, blocking calls on async paths, N+1 / repeated I/O, and \
               accidental quadratic behavior introduced by this diff. Still report anything \
               clearly severe you notice outside this lens.",
    },
    ReviewDimension {
        id: "tests_contracts",
        display: "Tests & contracts",
        lens: "\n\n## This review's lens: TESTS & CONTRACTS\nConcentrate on whether the change is \
               covered by tests, whether public APIs/contracts stay consistent, and whether the \
               diff changes a convention on its lines while leaving sibling/parallel code on the \
               old form (a one-sided divergence). Still report anything clearly severe you notice \
               outside this lens.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_table_is_the_four_expected_lenses() {
        let ids: Vec<_> = REVIEW_DIMENSIONS.iter().map(|d| d.id).collect();
        assert_eq!(
            ids,
            ["correctness", "security", "performance", "tests_contracts"]
        );
        for d in REVIEW_DIMENSIONS {
            assert!(!d.display.is_empty(), "{} display", d.id);
            assert!(
                d.lens.contains("This review's lens"),
                "{} lens must be an appendable section",
                d.id
            );
        }
    }
}
