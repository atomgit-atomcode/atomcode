//! Deep-mode dimension fan-out for `code_review`: many read-only reviewers, one
//! per concern lens, merged into a single deduped finding set. See
//! docs/plans/2026-08-20-code-review-deep-mode-fanout-design.md.

use std::cmp::Ordering;

use crate::review_tool::{cmp_finding, paths_match};
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

/// Result of running one dimension reviewer. `completed` is true only on a clean
/// finish (agent stopped, no error); a cancelled/errored dimension still
/// contributes whatever findings it already reported.
pub struct DimensionOutcome {
    pub dimension: &'static str,
    pub findings: Vec<Finding>,
    pub completed: bool,
    pub error: Option<String>,
}

/// Merge → scope-filter → sort → render the deep-review outcomes. Returns
/// `(is_error, rendered)`. `is_error` is true only when NO dimension completed
/// cleanly (a fully failed fan-out); a partial run renders its findings and notes
/// coverage.
pub fn finalize_deep_review(
    outcomes: &[DimensionOutcome],
    changed_files: usize,
    changed_paths: &[String],
) -> (bool, String) {
    // Feed merge in stable dimension order regardless of completion order.
    let per_dim: Vec<(&'static str, Vec<Finding>)> = REVIEW_DIMENSIONS
        .iter()
        .filter_map(|d| {
            outcomes
                .iter()
                .find(|o| o.dimension == d.id)
                .map(|o| (d.id, o.findings.clone()))
        })
        .collect();
    let raw_total: usize = per_dim.iter().map(|(_, v)| v.len()).sum();
    let mut merged = merge_findings(per_dim);
    merged.retain(|m| {
        changed_paths
            .iter()
            .any(|cf| paths_match(cf, &m.finding.file_path))
    });
    merged.sort_by(|a, b| cmp_finding(&a.finding, &b.finding));
    let deduped = raw_total.saturating_sub(merged.len());

    let completed: Vec<&'static str> = REVIEW_DIMENSIONS
        .iter()
        .filter(|d| outcomes.iter().any(|o| o.dimension == d.id && o.completed))
        .map(|d| d.id)
        .collect();
    let failed: Vec<&'static str> = REVIEW_DIMENSIONS
        .iter()
        .filter(|d| outcomes.iter().any(|o| o.dimension == d.id && !o.completed))
        .map(|d| d.id)
        .collect();

    let is_error = completed.is_empty();
    let rendered = render_deep(&merged, changed_files, &completed, &failed, deduped, is_error);
    (is_error, rendered)
}

fn render_deep(
    merged: &[MergedFinding],
    changed_files: usize,
    completed: &[&str],
    failed: &[&str],
    deduped: usize,
    is_error: bool,
) -> String {
    let total_dims = REVIEW_DIMENSIONS.len();
    let mut out = String::new();
    if is_error {
        out.push_str(&format!(
            "Deep review incomplete — every dimension failed (0/{total_dims}). \
             Coverage is not reliable.\n"
        ));
    } else if merged.is_empty() {
        out.push_str(&format!(
            "Deep review complete — no issues found across {changed_files} changed file(s) \
             ({}/{total_dims} dimensions completed).\n",
            completed.len()
        ));
    } else {
        out.push_str(&format!(
            "Deep review: {} finding(s) across {changed_files} changed file(s) · \
             {}/{total_dims} dimensions completed",
            merged.len(),
            completed.len()
        ));
        if deduped > 0 {
            out.push_str(&format!(" · deduped {deduped}"));
        }
        out.push('\n');
    }
    if !failed.is_empty() {
        out.push_str(&format!("Failed dimensions: {}\n", failed.join(", ")));
    }
    for (i, m) in merged.iter().enumerate() {
        let f = &m.finding;
        out.push_str(&format!(
            "\n{}. [{} · conf {:.2}] {}:{}-{} · dims: {}\n   {}\n",
            i + 1,
            f.priority,
            f.confidence,
            f.file_path,
            f.line_start,
            f.line_end,
            m.dimensions.join(","),
            f.title.trim()
        ));
        if !f.body.trim().is_empty() {
            out.push_str(&format!("   {}\n", f.body.trim().replace('\n', "\n   ")));
        }
        if !f.suggestion.trim().is_empty() {
            out.push_str(&format!("   ↳ fix: {}\n", f.suggestion.trim().replace('\n', "\n   ")));
        }
    }
    out
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

    fn outcome(dim: &'static str, completed: bool, findings: Vec<Finding>) -> DimensionOutcome {
        DimensionOutcome {
            dimension: dim,
            findings,
            completed,
            error: (!completed).then(|| "boom".to_string()),
        }
    }

    #[test]
    fn finalize_merges_filters_to_changed_files_and_sorts() {
        let outcomes = vec![
            outcome("correctness", true, vec![f("P2", 0.7, "a.rs", 5, 6, "clone in loop")]),
            outcome("security", true, vec![
                f("P0", 0.9, "a.rs", 1, 1, "hardcoded secret"),
                f("P1", 0.8, "not_changed.rs", 1, 1, "ignored"),
            ]),
        ];
        let (is_error, out) = finalize_deep_review(&outcomes, 1, &["a.rs".to_string()]);
        assert!(!is_error);
        // P0 sorts before P2; the non-changed-file finding is dropped.
        let p0 = out.find("hardcoded secret").unwrap();
        let p2 = out.find("clone in loop").unwrap();
        assert!(p0 < p2, "P0 must render before P2:\n{out}");
        assert!(!out.contains("ignored"), "off-scope finding filtered:\n{out}");
        assert!(out.contains("2/4"), "dimension completion summary:\n{out}");
    }

    #[test]
    fn finalize_flags_error_only_when_no_dimension_completed() {
        let all_failed = vec![
            outcome("correctness", false, vec![]),
            outcome("security", false, vec![]),
            outcome("performance", false, vec![]),
            outcome("tests_contracts", false, vec![]),
        ];
        let (is_error, out) = finalize_deep_review(&all_failed, 1, &["a.rs".to_string()]);
        assert!(is_error, "every dimension failed → hard error");
        assert!(out.contains("incomplete") || out.contains("0/4"), "{out}");

        let one_ok = vec![
            outcome("correctness", true, vec![]),
            outcome("security", false, vec![]),
            outcome("performance", false, vec![]),
            outcome("tests_contracts", false, vec![]),
        ];
        let (is_error, _) = finalize_deep_review(&one_ok, 1, &["a.rs".to_string()]);
        assert!(!is_error, "one clean dimension → partial but not a hard error");
    }

    #[test]
    fn finalize_tags_findings_with_contributing_dimensions() {
        let outcomes = vec![
            outcome("correctness", true, vec![f("P1", 0.8, "a.rs", 3, 4, "bad unwrap")]),
            outcome("security", true, vec![f("P1", 0.8, "a.rs", 3, 4, "bad unwrap")]),
        ];
        let (_, out) = finalize_deep_review(&outcomes, 1, &["a.rs".to_string()]);
        assert!(out.contains("correctness") && out.contains("security"), "dims tagged:\n{out}");
    }
}
