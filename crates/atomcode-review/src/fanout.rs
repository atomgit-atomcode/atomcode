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
    let (merged, deduped) = merge_deep_findings(outcomes, changed_paths);
    let (completed, failed) = dimension_coverage(outcomes);
    render_deep_result(&merged, changed_files, &completed, &failed, deduped, None)
}

/// Merge → scope-filter → sort the fan-out outcomes into the deduped survivor
/// set, plus the count collapsed by dedup. Shared by the deep and deep+verify
/// paths.
pub fn merge_deep_findings(
    outcomes: &[DimensionOutcome],
    changed_paths: &[String],
) -> (Vec<MergedFinding>, usize) {
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
    (merged, deduped)
}

/// Completed vs failed dimension ids, in table order. A dimension absent from
/// `outcomes` counts as neither — but never enters `completed`, so `is_error`
/// stays correct.
pub fn dimension_coverage(
    outcomes: &[DimensionOutcome],
) -> (Vec<&'static str>, Vec<&'static str>) {
    let completed = REVIEW_DIMENSIONS
        .iter()
        .filter(|d| outcomes.iter().any(|o| o.dimension == d.id && o.completed))
        .map(|d| d.id)
        .collect();
    let failed = REVIEW_DIMENSIONS
        .iter()
        .filter(|d| outcomes.iter().any(|o| o.dimension == d.id && !o.completed))
        .map(|d| d.id)
        .collect();
    (completed, failed)
}

/// Render the merged (post-verify, if any) survivor set. `verify_dropped` adds a
/// "verify dropped K" note when `Some`. Returns `(is_error, rendered)`;
/// `is_error` is true only when no dimension completed cleanly.
pub fn render_deep_result(
    merged: &[MergedFinding],
    changed_files: usize,
    completed: &[&str],
    failed: &[&str],
    deduped: usize,
    verify_dropped: Option<usize>,
) -> (bool, String) {
    let is_error = completed.is_empty();
    let rendered = render_deep(
        merged,
        changed_files,
        completed,
        failed,
        deduped,
        is_error,
        verify_dropped,
    );
    (is_error, rendered)
}

fn render_deep(
    merged: &[MergedFinding],
    changed_files: usize,
    completed: &[&str],
    failed: &[&str],
    deduped: usize,
    is_error: bool,
    verify_dropped: Option<usize>,
) -> String {
    let total_dims = REVIEW_DIMENSIONS.len();
    let verify_note = match verify_dropped {
        Some(k) => format!(" · verify dropped {k}"),
        None => String::new(),
    };
    let mut out = String::new();
    if is_error {
        out.push_str(&format!(
            "Deep review incomplete — every dimension failed (0/{total_dims}). \
             Coverage is not reliable.{verify_note}\n"
        ));
    } else if merged.is_empty() {
        out.push_str(&format!(
            "Deep review complete — no issues found across {changed_files} changed file(s) \
             ({}/{total_dims} dimensions completed){verify_note}.\n",
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
        out.push_str(&verify_note);
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

/// Concurrency cap for the verify pass (one agent per surviving finding).
pub const VERIFY_CONCURRENCY: usize = 6;

/// Persona lens appended to the base reviewer persona for a verify agent.
pub const VERIFY_LENS: &str = "\n\n## This review's task: VERIFY ONE CANDIDATE FINDING\n\
You are checking a single candidate finding from a prior review pass. Using the DIFF as the \
authoritative source (plus read-only tools for context), decide whether it is a REAL defect \
INTRODUCED by these changes. If it is real — OR if you are unsure — call `report_finding` to \
re-report it (you may refine its wording). Report NOTHING only when you are confident it is a \
false positive, is not introduced by this diff, or is already handled, and briefly say why. Do \
not hunt for new, unrelated issues.";

/// Build the single-finding task text handed to a verify agent: the candidate,
/// the per-language rules, and the authoritative DIFF.
pub fn render_verify_task(f: &Finding, rules: &str, annotated: &str) -> String {
    format!(
        "Verify the following single candidate finding from a prior review.\n\n\
         CANDIDATE FINDING:\n\
         - [{} · conf {:.2}] {}:{}-{}\n  {}\n  {}\n\n{rules}\n\n=== DIFF ===\n{annotated}",
        f.priority,
        f.confidence,
        f.file_path,
        f.line_start,
        f.line_end,
        f.title.trim(),
        f.body.trim(),
    )
}

/// Run up to `cap` verify checks concurrently over `n` items; return a keep-mask
/// in index order. `verify_one(i)` yields `(i, keep)`. The default is `true`
/// (fail-open): a panicked/aborted verify task leaves its finding kept.
pub async fn run_verify<F, Fut>(n: usize, cap: usize, verify_one: F) -> Vec<bool>
where
    F: Fn(usize) -> Fut,
    Fut: std::future::Future<Output = (usize, bool)> + Send + 'static,
{
    let cap = cap.max(1);
    let mut keep = vec![true; n];
    let mut set = tokio::task::JoinSet::new();
    let mut next = 0usize;
    while next < n && set.len() < cap {
        set.spawn(verify_one(next));
        next += 1;
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((i, kept)) = joined {
            if i < n {
                keep[i] = kept;
            }
        }
        if next < n {
            set.spawn(verify_one(next));
            next += 1;
        }
    }
    keep
}

/// Run every dimension concurrently and collect their outcomes in `dims` order.
/// `run_one` builds and drives one dimension's reviewer; it must return a
/// `Send + 'static` future (the production runner clones everything it needs).
pub async fn run_deep_review<F, Fut>(
    dims: &'static [ReviewDimension],
    run_one: F,
) -> Vec<DimensionOutcome>
where
    F: Fn(&'static ReviewDimension) -> Fut,
    Fut: std::future::Future<Output = DimensionOutcome> + Send + 'static,
{
    let mut set = tokio::task::JoinSet::new();
    for dim in dims {
        set.spawn(run_one(dim));
    }
    let mut collected: Vec<DimensionOutcome> = Vec::with_capacity(dims.len());
    while let Some(joined) = set.join_next().await {
        if let Ok(outcome) = joined {
            collected.push(outcome);
        }
        // A panicked/aborted task is simply absent; finalize treats a missing
        // dimension as not-completed (it never appears in `completed`).
    }
    // Return in stable dimension order regardless of completion order.
    dims.iter()
        .filter_map(|d| {
            collected
                .iter()
                .position(|o| o.dimension == d.id)
                .map(|i| collected.remove(i))
        })
        .collect()
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

    #[test]
    fn finalize_output_is_unchanged_after_the_split() {
        // The no-verify path must be byte-identical to Phase 1's behavior.
        let outcomes = vec![
            DimensionOutcome { dimension: "correctness", findings: vec![f("P1", 0.8, "a.rs", 3, 4, "bad unwrap")], completed: true, error: None },
            DimensionOutcome { dimension: "security", findings: vec![], completed: true, error: None },
            DimensionOutcome { dimension: "performance", findings: vec![], completed: false, error: Some("boom".into()) },
            DimensionOutcome { dimension: "tests_contracts", findings: vec![], completed: true, error: None },
        ];
        let (err_a, out_a) = finalize_deep_review(&outcomes, 1, &["a.rs".to_string()]);
        let (merged, deduped) = merge_deep_findings(&outcomes, &["a.rs".to_string()]);
        let (completed, failed) = dimension_coverage(&outcomes);
        let (err_b, out_b) = render_deep_result(&merged, 1, &completed, &failed, deduped, None);
        assert_eq!((err_a, out_a), (err_b, out_b), "finalize must equal its decomposed form with verify_dropped=None");
    }

    #[test]
    fn render_deep_result_notes_verify_dropped() {
        let merged = vec![]; // all survivors culled
        let (_e, out) = render_deep_result(&merged, 1, &["correctness"], &[], 0, Some(2));
        assert!(out.contains("verify"), "verify note present: {out}");
        assert!(out.contains('2'), "dropped count present: {out}");
    }

    #[tokio::test]
    async fn run_verify_applies_keep_mask_in_order_with_a_small_cap() {
        // Keep evens, drop odds; cap < n exercises the refill path.
        let keep = run_verify(5, 2, |i| async move { (i, i % 2 == 0) }).await;
        assert_eq!(keep, vec![true, false, true, false, true]);
    }

    #[test]
    fn verify_task_embeds_the_candidate_and_diff() {
        let finding = f("P1", 0.9, "a.rs", 10, 12, "unchecked unwrap");
        let task = render_verify_task(&finding, "RULES-HERE", "DIFF-HERE");
        assert!(task.contains("unchecked unwrap") && task.contains("a.rs:10-12"));
        assert!(task.contains("RULES-HERE") && task.contains("DIFF-HERE"));
        assert!(task.to_lowercase().contains("verify"));
    }

    #[tokio::test]
    async fn run_deep_review_runs_all_dimensions_and_preserves_order() {
        let outcomes = run_deep_review(REVIEW_DIMENSIONS, |dim| {
            let id = dim.id;
            async move {
                DimensionOutcome {
                    dimension: id,
                    findings: vec![f("P2", 0.7, "a.rs", 1, 1, id)],
                    completed: true,
                    error: None,
                }
            }
        })
        .await;
        let ids: Vec<_> = outcomes.iter().map(|o| o.dimension).collect();
        assert_eq!(ids, ["correctness", "security", "performance", "tests_contracts"]);
        assert!(outcomes.iter().all(|o| o.completed && o.findings.len() == 1));
    }
}
