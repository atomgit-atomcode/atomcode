//! Deep-mode dimension fan-out for `code_review`: many read-only reviewers, one
//! per concern lens, merged into a single deduped finding set. See
//! docs/plans/2026-08-20-code-review-deep-mode-fanout-design.md.

use std::cmp::Ordering;

use crate::Finding;

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

/// A finding that survived dedup, tagged with every dimension that reported it.
pub struct MergedFinding {
    pub finding: Finding,
    pub dimensions: Vec<&'static str>,
}

/// Collapse per-dimension findings into a deduped set. Two findings are the same
/// issue when they touch the same file, their line ranges overlap, and their
/// titles are similar. On a collision the higher-priority (then higher-confidence)
/// finding's content is kept and every contributing dimension is credited.
pub fn merge_findings(per_dim: Vec<(&'static str, Vec<Finding>)>) -> Vec<MergedFinding> {
    let mut merged: Vec<MergedFinding> = Vec::new();
    for (dim, findings) in per_dim {
        for finding in findings {
            match merged.iter_mut().find(|m| is_duplicate(&m.finding, &finding)) {
                Some(existing) => {
                    if !existing.dimensions.contains(&dim) {
                        existing.dimensions.push(dim);
                    }
                    if outranks(&finding, &existing.finding) {
                        existing.finding = finding;
                    }
                }
                None => merged.push(MergedFinding {
                    finding,
                    dimensions: vec![dim],
                }),
            }
        }
    }
    merged
}

fn is_duplicate(a: &Finding, b: &Finding) -> bool {
    same_file(&a.file_path, &b.file_path)
        && ranges_overlap(a.line_start, a.line_end, b.line_start, b.line_end)
        && titles_similar(&a.title, &b.title)
}

fn same_file(a: &str, b: &str) -> bool {
    let na = a.trim_start_matches("./");
    let nb = b.trim_start_matches("./");
    na == nb || na.ends_with(nb) || nb.ends_with(na)
}

fn ranges_overlap(a0: u32, a1: u32, b0: u32, b1: u32) -> bool {
    a0 <= b1 && b0 <= a1
}

/// Title similarity by token-set Jaccard (≥ 0.5), case/punctuation-insensitive.
/// Empty token sets fall back to trimmed case-insensitive equality.
fn titles_similar(a: &str, b: &str) -> bool {
    let ta = title_tokens(a);
    let tb = title_tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return a.trim().eq_ignore_ascii_case(b.trim());
    }
    let inter = ta.iter().filter(|t| tb.contains(*t)).count();
    let union = ta.len() + tb.len() - inter;
    union > 0 && (inter as f32 / union as f32) >= 0.5
}

fn title_tokens(s: &str) -> std::collections::BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Priority ascending (`P0` most severe), then confidence descending.
fn outranks(candidate: &Finding, current: &Finding) -> bool {
    match candidate.priority.cmp(&current.priority) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => candidate.confidence > current.confidence,
    }
}

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

    fn f(priority: &str, conf: f32, file: &str, ls: u32, le: u32, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: String::new(),
            priority: priority.into(),
            confidence: conf,
            file_path: file.into(),
            line_start: ls,
            line_end: le,
            suggestion: String::new(),
            suggested_code: String::new(),
        }
    }

    #[test]
    fn merge_dedups_same_file_overlapping_range_and_similar_title() {
        let merged = merge_findings(vec![
            ("correctness", vec![f("P1", 0.8, "a.rs", 10, 12, "unchecked unwrap on None")]),
            ("security", vec![f("P2", 0.6, "a.rs", 11, 15, "unwrap on None value")]),
        ]);
        assert_eq!(merged.len(), 1, "overlapping near-duplicate collapses");
        // Higher-priority (P1) content wins; both dimensions are credited.
        assert_eq!(merged[0].finding.priority, "P1");
        assert_eq!(merged[0].dimensions, vec!["correctness", "security"]);
    }

    #[test]
    fn merge_keeps_distinct_findings() {
        let merged = merge_findings(vec![
            ("correctness", vec![f("P1", 0.8, "a.rs", 10, 12, "unchecked unwrap")]),
            ("performance", vec![f("P2", 0.7, "a.rs", 90, 92, "needless clone in loop")]),
            ("security", vec![f("P1", 0.9, "b.rs", 10, 12, "unchecked unwrap")]),
        ]);
        assert_eq!(merged.len(), 3, "different range or file are not duplicates");
    }

    #[test]
    fn merge_prefers_higher_confidence_when_priority_ties() {
        let merged = merge_findings(vec![
            ("correctness", vec![f("P2", 0.5, "a.rs", 1, 1, "same bug title")]),
            ("security", vec![f("P2", 0.9, "a.rs", 1, 1, "same bug title")]),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].finding.confidence, 0.9);
    }
}
