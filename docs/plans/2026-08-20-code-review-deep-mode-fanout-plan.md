# code-review deep mode (dimension fan-out) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `/review deep` mode that fans out one read-only reviewer per concern dimension (correctness / security / performance / tests&contracts), runs them concurrently, and merges/dedups their findings — while the default `/review` keeps running a single agent unchanged.

**Architecture:** A new `atomcode-review/src/fanout.rs` owns a fixed dimension table (each dimension is a `persona_append` lens), a pure `merge_findings` deduplicator, a pure deep-review renderer, and a generic `run_deep_review` orchestrator (concurrency via `tokio::task::JoinSet`, injectable per-dimension runner for tests). `ReviewTool::execute` gains a `depth` arg and dispatches: `single` (today's exact path, untouched) vs `deep` (build N dimension agents via `build_review_agent_with` + `ReviewAgentConfig::with_persona_append`, each with its own `ReportFindingTool` sink, then merge → scope-filter → render).

**Tech Stack:** Rust, tokio (`rt-multi-thread`, `macros`, `time`, `sync` — already enabled), `atomcode-kernel` Agent, `atomcode-capabilities` `Finding`/`ReportFindingTool`. No new dependencies.

**Spec:** `docs/plans/2026-08-20-code-review-deep-mode-fanout-design.md`

## Global Constraints

- No new crate dependencies. `futures` is dev-only in `atomcode-review`, so production orchestration MUST use `tokio::task::JoinSet` (NOT `futures::future::join_all`).
- The single-agent path (`depth` absent or `"single"`) MUST be behavior-identical to today. All existing `atomcode-review` and `atomcode-tuix` tests stay green unchanged.
- Deep mode is opt-in only (`/review deep` / tool arg `depth:"deep"`). Default stays single.
- Scope preflight/confirmation runs BEFORE any fan-out (reuse the existing `ScopeManifest`/`ScopeLimits` block in `execute()`).
- Review findings render in English (match the existing `render_findings` output).
- `Finding` fields (from `atomcode-capabilities`, do NOT modify): `title: String`, `body: String`, `priority: String` (`"P0"`..`"P3"`, 0 most severe), `confidence: f32` (0.0..=1.0), `file_path: String`, `line_start: u32`, `line_end: u32`, `suggestion: String`, `suggested_code: String`.

---

### Task 1: Dimension table + module wiring

**Files:**
- Create: `crates/atomcode-review/src/fanout.rs`
- Modify: `crates/atomcode-review/src/lib.rs` (declare + export the module)
- Test: in `fanout.rs` `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub struct ReviewDimension { pub id: &'static str, pub display: &'static str, pub lens: &'static str }`
  - `pub const REVIEW_DIMENSIONS: &[ReviewDimension]` (4 entries: `correctness`, `security`, `performance`, `tests_contracts`)

- [ ] **Step 1: Write the failing test**

In a new `crates/atomcode-review/src/fanout.rs`, put the table + this test:

```rust
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
```

Then wire the module in `crates/atomcode-review/src/lib.rs`: add `pub mod fanout;` next to the other `pub mod` lines, and add to the re-export list:

```rust
pub use fanout::{ReviewDimension, REVIEW_DIMENSIONS};
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-review --lib fanout::tests::dimension_table_is_the_four_expected_lenses`
Expected: FAIL to compile first (module not declared) → after declaring, PASS. If it compiles and fails, the table is wrong; fix until it fails only for a real reason. (This task is mostly data; once the file + module wiring exist it passes.)

- [ ] **Step 3: Write minimal implementation**

Already written in Step 1 (the table IS the implementation). Ensure `lib.rs` declares `pub mod fanout;` and the re-export compiles.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-review --lib fanout`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/fanout.rs crates/atomcode-review/src/lib.rs
git commit -m "feat(review): deep-mode dimension table (fanout scaffolding)"
```

---

### Task 2: `merge_findings` deduplicator (pure)

**Files:**
- Modify: `crates/atomcode-review/src/fanout.rs`
- Test: in `fanout.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Finding` (`crate::Finding`).
- Produces:
  - `pub struct MergedFinding { pub finding: Finding, pub dimensions: Vec<&'static str> }`
  - `pub fn merge_findings(per_dim: Vec<(&'static str, Vec<Finding>)>) -> Vec<MergedFinding>`

- [ ] **Step 1: Write the failing test**

Add to `fanout.rs` (top-of-file `use`):

```rust
use std::cmp::Ordering;

use crate::Finding;
```

Add the test module cases (inside the existing `mod tests`):

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-review --lib fanout::tests::merge_`
Expected: FAIL to compile — `merge_findings` / `MergedFinding` not defined.

- [ ] **Step 3: Write minimal implementation**

Add to `fanout.rs` (module body, above `#[cfg(test)]`):

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-review --lib fanout::tests::merge_`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/fanout.rs
git commit -m "feat(review): merge_findings deduplicator for deep mode"
```

---

### Task 3: Shared finding comparator + deep renderer (pure)

**Files:**
- Modify: `crates/atomcode-review/src/review_tool.rs` (extract `cmp_finding`, make `paths_match` reusable)
- Modify: `crates/atomcode-review/src/fanout.rs` (add `DimensionOutcome`, `finalize_deep_review`, `render_deep`)
- Test: in `fanout.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `merge_findings`, `MergedFinding`, `REVIEW_DIMENSIONS`, `crate::review_tool::{cmp_finding, paths_match}`.
- Produces:
  - `pub struct DimensionOutcome { pub dimension: &'static str, pub findings: Vec<Finding>, pub completed: bool, pub error: Option<String> }`
  - `pub fn finalize_deep_review(outcomes: &[DimensionOutcome], changed_files: usize, changed_paths: &[String]) -> (bool, String)` — returns `(is_error, rendered)`. `is_error` is true only when NO dimension completed cleanly.

- [ ] **Step 1: Write the failing test**

First, in `crates/atomcode-review/src/review_tool.rs`, extract the comparator and widen visibility so `fanout` can reuse them. Replace the body of `sort_findings` and expose `cmp_finding` + `paths_match`:

```rust
/// Priority ascending (`P0` most severe) then confidence descending. Shared with
/// deep-mode merge ordering.
pub(crate) fn cmp_finding(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    a.priority.cmp(&b.priority).then(
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal),
    )
}

fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| cmp_finding(a, b));
}
```

and change `fn paths_match(` to `pub(crate) fn paths_match(` (line ~668).

Then add these tests to `fanout.rs` `mod tests`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-review --lib fanout::tests::finalize_`
Expected: FAIL to compile — `DimensionOutcome` / `finalize_deep_review` not defined.

- [ ] **Step 3: Write minimal implementation**

Add to `fanout.rs` (module body). Add the imports it needs at the top of the file:

```rust
use crate::review_tool::{cmp_finding, paths_match};
```

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-review --lib fanout::tests::finalize_` then `cargo test -p atomcode-review --lib` (ensure `sort_findings`/`paths_match` refactor kept existing tests green).
Expected: PASS; no regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/fanout.rs crates/atomcode-review/src/review_tool.rs
git commit -m "feat(review): deep-review finalize/merge/render + shared cmp_finding"
```

---

### Task 4: `run_deep_review` orchestrator (concurrent, injectable)

**Files:**
- Modify: `crates/atomcode-review/src/fanout.rs`
- Test: in `fanout.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `ReviewDimension`, `REVIEW_DIMENSIONS`, `DimensionOutcome`.
- Produces:
  - `pub async fn run_deep_review<F, Fut>(dims: &'static [ReviewDimension], run_one: F) -> Vec<DimensionOutcome>` where `F: Fn(&'static ReviewDimension) -> Fut`, `Fut: std::future::Future<Output = DimensionOutcome> + Send + 'static`. Results are returned in `dims` order regardless of completion order.

- [ ] **Step 1: Write the failing test**

Add to `fanout.rs` `mod tests`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-review --lib fanout::tests::run_deep_review_runs_all`
Expected: FAIL to compile — `run_deep_review` not defined.

- [ ] **Step 3: Write minimal implementation**

Add to `fanout.rs`. Uses `tokio::task::JoinSet` (no `futures` dep). Because `JoinSet` yields in completion order, reorder by dimension index before returning:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-review --lib fanout::tests::run_deep_review_runs_all`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/fanout.rs
git commit -m "feat(review): run_deep_review concurrent orchestrator (JoinSet)"
```

---

### Task 5: Wire `depth` into `code_review` execute + tool schema

**Files:**
- Modify: `crates/atomcode-review/src/review_tool.rs` (`Args.depth`, `is_deep`, schema, `execute` dispatch, real per-dimension runner)
- Test: in `review_tool.rs` `#[cfg(test)]` (reuse `ScriptedReviewProvider`)

**Interfaces:**
- Consumes: `fanout::{run_deep_review, finalize_deep_review, DimensionOutcome, REVIEW_DIMENSIONS}`, `build_review_agent_with`, `ReviewAgentConfig::with_persona_append`.
- Produces: `code_review` accepting `{"depth":"deep"}` and running the fan-out; default/`"single"` unchanged.

- [ ] **Step 1: Write the failing test**

Add to `review_tool.rs` `mod tests` (the `ScriptedReviewProvider` reports one `unchecked unwrap` finding at `a.rs:1` per agent; deep runs 4 agents → 4 identical findings → dedup to 1):

```rust
    #[test]
    fn args_parse_depth_field() {
        let d: Args = serde_json::from_str(r#"{"depth":"deep"}"#).unwrap();
        assert!(d.is_deep());
        let s: Args = serde_json::from_str("{}").unwrap();
        assert!(!s.is_deep());
        let explicit: Args = serde_json::from_str(r#"{"depth":"single"}"#).unwrap();
        assert!(!explicit.is_deep());
    }

    #[tokio::test]
    async fn deep_review_fans_out_and_dedups_across_dimensions() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig {
                model: "mock-model".into(),
                ..Default::default()
            },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let res = tool.execute(r#"{"depth":"deep"}"#, &ctx).await;

        assert!(!res.is_error, "deep review should succeed: {}", res.content);
        assert!(
            res.content.contains("Deep review"),
            "deep header present: {}",
            res.content
        );
        // All four dimensions report the same finding → merged to ONE.
        assert!(
            res.content.contains("1 finding(s)") || res.content.contains("1 finding"),
            "identical findings across dimensions must dedup to one: {}",
            res.content
        );
        assert!(
            res.content.contains("dims:"),
            "merged finding is tagged with its dimensions: {}",
            res.content
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-review --lib deep_review_fans_out`
Expected: FAIL to compile — `Args::is_deep` not defined / `depth` field missing.

- [ ] **Step 3: Write minimal implementation**

(a) Add the field to `struct Args` (after `confirm_scope`):

```rust
    /// Review depth. `"deep"` fans out one read-only reviewer per concern
    /// dimension and merges their findings; absent / `"single"` runs the default
    /// single reviewer. Unknown values fall back to single.
    #[serde(default)]
    depth: Option<String>,
```

and add the helper in `impl Args`:

```rust
    fn is_deep(&self) -> bool {
        self.depth
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("deep"))
            .unwrap_or(false)
    }
```

(b) Add `depth` to the tool schema `parameters()` `properties` (next to `confirm_scope`, line ~421):

```rust
                "depth": { "type": "string", "enum": ["single", "deep"], "description": "Review depth. `deep` fans out one reviewer per concern dimension (correctness/security/performance/tests) and merges findings; omit for the default single reviewer." }
```

(c) Import the fanout entry points at the top of `review_tool.rs`:

```rust
use crate::fanout::{finalize_deep_review, run_deep_review, DimensionOutcome, REVIEW_DIMENSIONS};
```

(d) In `execute()`, replace the single-agent block (steps 3–5, the current lines that build one `cfg`, call `build_review_agent_with`, `tokio::select!`, and render) with a dispatch. Keep the shared prep (`annotated`, `files`, `rules`, `impact_plan`, `task`, `provider`) exactly as-is, then:

```rust
        // Shared per-agent config seed (both paths).
        let make_cfg = || {
            let mut cfg = ReviewAgentConfig::new("", "", &self.cfg.model, &ctx.working_dir);
            cfg.context_window = self.cfg.context_window;
            cfg.stream_timeout = self.cfg.stream_timeout;
            cfg.request_timeout = self.cfg.request_timeout;
            cfg.max_rounds = self.max_rounds;
            cfg.max_turn_duration = self.max_turn_duration;
            cfg.tool_loop_policy = self.tool_loop_policy;
            cfg.progress = Some(ctx.progress.clone());
            cfg.review_paths = files.clone();
            cfg
        };

        if !a.is_deep() {
            // --- single-agent path (unchanged behavior) ---
            let (agent, report) = build_review_agent_with(&make_cfg(), provider);
            let (stop, run_error) = tokio::select! {
                _ = ctx.cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
                    (outcome.stop, outcome.error)
                }
            };
            let mut findings = report.findings();
            findings.retain(|f| files.iter().any(|cf| paths_match(cf, &f.file_path)));
            sort_findings(&mut findings);
            return if stop == StopReason::Stopped && run_error.is_none() {
                ok(render_findings(&findings, files.len()))
            } else {
                err(render_incomplete_review(&findings, files.len(), stop, run_error.as_deref()))
            };
        }

        // --- deep fan-out path ---
        let outcomes = run_deep_review(REVIEW_DIMENSIONS, |dim| {
            // Clone everything so each dimension future is Send + 'static.
            let provider = provider.clone();
            let task = task.clone();
            let mut cfg = make_cfg();
            let cancel = ctx.cancel.clone();
            async move {
                cfg = cfg.with_persona_append(dim.lens);
                let (agent, report) = build_review_agent_with(&cfg, provider);
                let (stop, run_error) = tokio::select! {
                    _ = cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                    outcome = agent.run_to_completion(task, AutoRespond::AllowAll) => {
                        (outcome.stop, outcome.error)
                    }
                };
                DimensionOutcome {
                    dimension: dim.id,
                    findings: report.findings(),
                    completed: stop == StopReason::Stopped && run_error.is_none(),
                    error: run_error,
                }
            }
        })
        .await;

        let (is_error, content) = finalize_deep_review(&outcomes, files.len(), &files);
        return if is_error { err(content) } else { ok(content) };
```

Note: `make_cfg` borrows `self`, `ctx`, `files`. The deep closure calls `make_cfg()` synchronously (before the `async move`) so the produced owned `cfg` is what moves into the future — no borrow of `self`/`ctx` crosses the await. Confirm this compiles; if the borrow checker complains about `make_cfg` in the closure, inline the cfg construction into the closure body BEFORE `async move` (build the owned `cfg` first, then `let cfg = cfg;` moved in).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-review --lib deep_review_fans_out args_parse_depth_field`
then the full crate: `cargo test -p atomcode-review`
Expected: PASS; the pre-existing single-path tests (`review_tool_reviews_a_real_diff`, round/duration tests) still green.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/review_tool.rs
git commit -m "feat(review): code_review deep depth arg → dimension fan-out"
```

---

### Task 6: `/review deep` command mapping

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs` (`review_prompt`)
- Test: in `commands.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: none new.
- Produces: `/review deep [scope]` synthesizes a tool call carrying `"depth":"deep"`; `/review [scope]` unchanged.

- [ ] **Step 1: Write the failing test**

Add to `commands.rs` `mod tests` (near `review_prompt_uses_explicit_tool_scopes`):

```rust
    #[test]
    fn review_prompt_deep_adds_depth_and_keeps_scope() {
        // `deep` alone → working-tree + depth.
        let wt = review_prompt("deep");
        assert!(wt.contains(r#""scope":{"kind":"working_tree"}"#), "{wt}");
        assert!(wt.contains(r#""depth":"deep""#), "{wt}");

        // `deep staged` → staged + depth.
        let st = review_prompt("deep staged");
        assert!(st.contains(r#""scope":{"kind":"staged"}"#), "{st}");
        assert!(st.contains(r#""depth":"deep""#), "{st}");

        // `deep <ref>` → range + depth.
        let rng = review_prompt("deep main");
        assert!(rng.contains(r#""scope":{"kind":"range","base":"main","head":"HEAD"}"#), "{rng}");
        assert!(rng.contains(r#""depth":"deep""#), "{rng}");

        // Plain scope carries NO depth (default single).
        assert!(!review_prompt("").contains("depth"));
        assert!(!review_prompt("staged").contains("depth"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix --lib review_prompt_deep_adds_depth`
Expected: FAIL (current `review_prompt` emits no `depth` and treats `deep` as a git ref).

- [ ] **Step 3: Write minimal implementation**

Replace `review_prompt` (commands.rs:90) with a version that parses a leading `deep` keyword and composes the tool-args object:

```rust
fn review_prompt(arg: &str) -> String {
    let arg = arg.trim();
    // A leading `deep` keyword (alone or before a scope) opts into deep mode.
    let (deep, scope) = match arg.strip_prefix("deep") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => (true, rest.trim()),
        _ => (false, arg),
    };
    let scope_json = if scope.is_empty() {
        r#"{"kind":"working_tree"}"#.to_string()
    } else if scope.eq_ignore_ascii_case("staged") {
        r#"{"kind":"staged"}"#.to_string()
    } else {
        format!(
            r#"{{"kind":"range","base":{base},"head":"HEAD"}}"#,
            base = serde_json::to_string(scope).expect("serializing a string cannot fail")
        )
    };
    let args = if deep {
        format!(r#"{{"scope":{scope_json},"depth":"deep"}}"#)
    } else {
        format!(r#"{{"scope":{scope_json}}}"#)
    };
    format!(
        "Review the requested changes: call the `code_review` tool with {args}, then give me a \
         concise summary of its findings."
    )
}
```

Note: this preserves the existing substrings the current tests assert (`{"scope":{"kind":"working_tree"}}`, `{"scope":{"kind":"staged"}}`, `{"scope":{"kind":"range","base":"release/v5.0.8","head":"HEAD"}}`, and JSON-escaped odd refs), so `review_prompt_uses_explicit_tool_scopes` and `review_prompt_json_escapes_the_base_ref` stay green.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-tuix --lib review_prompt`
Expected: PASS (new deep test + the two existing review_prompt tests).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(tuix): /review deep opts code_review into dimension fan-out"
```

---

### Task 7: Full-suite regression + docs note

**Files:**
- Modify: `crates/atomcode-review/src/review_tool.rs` (module-level doc note on deep mode — 2 lines)
- No new tests (verification task).

- [ ] **Step 1: Add the doc note**

At the end of the `review_tool.rs` module header (after line ~12), add:

```rust
//! Deep mode: passing `{"depth":"deep"}` fans out one read-only reviewer per
//! concern dimension (see `crate::fanout`) and merges/dedups their findings; the
//! default single-reviewer path is unchanged.
```

- [ ] **Step 2: Run the full relevant suites**

Run:
```bash
cargo test -p atomcode-review
cargo test -p atomcode-tuix --lib
cargo build -p atomcode-review -p atomcode-tuix
```
Expected: all green, zero warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-review/src/review_tool.rs
git commit -m "docs(review): note deep-mode fan-out in code_review header"
```

---

## Self-Review

**Spec coverage:**
- Interface `/review deep` + tool `depth` arg → Task 5 (arg/schema/dispatch), Task 6 (command).
- Dimension fan-out (correctness/security/performance/tests_contracts, full-diff lens via `persona_append`) → Task 1 (table), Task 5 (runner uses `with_persona_append`).
- Concurrent run, cancellable, `JoinSet` (no `futures` prod dep) → Task 4, Task 5.
- Merge/dedup by (file + overlapping range + similar title), keep higher priority/confidence, accumulate dimension tags → Task 2.
- Scope preflight BEFORE fan-out → Task 5 keeps the existing `ScopeManifest` block ahead of dispatch (shared prep untouched).
- Error handling: partial dimensions still contribute; hard error only when none completed → Task 3 (`finalize_deep_review` is_error rule) + Task 5 (`completed` flag).
- Reporting: reuse sort + per-dimension summary + dimension tags → Task 3 (`render_deep`).
- Default single path unchanged → Task 5 dispatch returns the original block verbatim.
- Phase-2 verify reserved (not built) → out of scope by design; `depth` enum can grow later.

**Placeholder scan:** none — every step has concrete code.

**Type consistency:** `Finding` fields match the capabilities struct verbatim; `cmp_finding`/`paths_match` are defined in Task 3 and consumed in Task 3's `finalize_deep_review`; `DimensionOutcome`/`MergedFinding`/`run_deep_review`/`finalize_deep_review` signatures are identical where produced (Tasks 2–4) and consumed (Task 5). `is_deep` defined and used in Task 5.
