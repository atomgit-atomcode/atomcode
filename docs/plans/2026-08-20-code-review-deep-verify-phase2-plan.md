# code-review deep+verify (Phase 2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `depth:"deep+verify"` that culls false positives from the deep-mode merged findings — one verify agent per surviving finding, single vote biased toward keep — while `single` and `deep` stay unchanged.

**Architecture:** Split Phase 1's `finalize_deep_review` in `atomcode-review/src/fanout.rs` into reusable pieces (`merge_deep_findings`, `dimension_coverage`, `render_deep_result` with an optional `verify_dropped` count); add a verifier persona lens, a bounded-concurrency `run_verify` keep-mask runner, and a `render_verify_task` helper. `review_tool.rs` gains `wants_verify()`, the schema enum value, and a `deep+verify` branch that runs the verify pass between merge and render. Verify reuses `build_review_agent_with` + `report_finding` — a verify agent that re-reports the finding means keep; reporting nothing means drop; error/cancel keeps (fail-open).

**Tech Stack:** Rust, tokio (`rt-multi-thread`, `sync`, `macros` — already enabled), `atomcode-kernel` Agent, `atomcode-capabilities` `Finding`/`ReportFindingTool`. No new dependencies.

**Spec:** `docs/plans/2026-08-20-code-review-deep-mode-fanout-design.md` (§ "Phase 2 — adversarial verify pass").

## Global Constraints

- No new crate dependencies. Concurrency uses `tokio::task::JoinSet` (NOT `futures`).
- `single` and `deep` paths stay behavior-identical. All existing `atomcode-review` and `atomcode-tuix` tests stay green unchanged; in particular `finalize_deep_review`'s output for the no-verify path must be byte-identical after the refactor (it delegates with `verify_dropped = None`).
- Verify is opt-in (`depth:"deep+verify"` only). Single vote is biased toward KEEP: a finding is dropped only when its verify agent completes cleanly AND re-reports nothing. Error/cancel/panic keeps the finding (fail-open).
- Scope preflight stays before fan-out (unchanged).
- Findings render in English.
- `Finding` fields (do NOT modify, from `atomcode-capabilities`): `title: String, body: String, priority: String ("P0".."P3"), confidence: f32, file_path: String, line_start: u32, line_end: u32, suggestion: String, suggested_code: String`.
- Current relevant landmarks (Phase 1, already merged): `fanout.rs` has `finalize_deep_review` (line ~151), `render_deep` (line ~192), `MergedFinding`/`DimensionOutcome`, `merge_findings`, `run_deep_review`; `review_tool.rs` has `Args.depth` + `is_deep()` (line ~307), the schema `depth` entry (line ~439), and the deep branch calling `finalize_deep_review` (line ~567). `annotated`, `files`, `rules`, `task` are already in scope at the deep branch.

---

### Task 1: Split finalize + add verifier lens, run_verify, verify-task helper (fanout.rs)

**Files:**
- Modify: `crates/atomcode-review/src/fanout.rs`
- Modify: `crates/atomcode-review/src/lib.rs` (export new items)
- Test: in `fanout.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `MergedFinding`, `DimensionOutcome`, `merge_findings`, `REVIEW_DIMENSIONS`, `crate::Finding`, `crate::review_tool::{cmp_finding, paths_match}` (already used in this file).
- Produces:
  - `pub fn merge_deep_findings(outcomes: &[DimensionOutcome], changed_paths: &[String]) -> (Vec<MergedFinding>, usize)`
  - `pub fn dimension_coverage(outcomes: &[DimensionOutcome]) -> (Vec<&'static str>, Vec<&'static str>)`
  - `pub fn render_deep_result(merged: &[MergedFinding], changed_files: usize, completed: &[&str], failed: &[&str], deduped: usize, verify_dropped: Option<usize>) -> (bool, String)`
  - `pub const VERIFY_LENS: &str`, `pub const VERIFY_CONCURRENCY: usize`
  - `pub fn render_verify_task(f: &Finding, rules: &str, annotated: &str) -> String`
  - `pub async fn run_verify<F, Fut>(n: usize, cap: usize, verify_one: F) -> Vec<bool>` where `F: Fn(usize) -> Fut`, `Fut: Future<Output = (usize, bool)> + Send + 'static`

- [ ] **Step 1: Write the failing tests**

Add to `fanout.rs` `mod tests` (the `f(...)` helper already exists there):

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-review --lib fanout::tests::finalize_output_is_unchanged_after_the_split fanout::tests::render_deep_result_notes_verify_dropped fanout::tests::run_verify_applies fanout::tests::verify_task_embeds`
Expected: FAIL to compile — the new functions/consts don't exist yet.

- [ ] **Step 3: Write the implementation**

In `fanout.rs`, replace the body of `finalize_deep_review` (currently lines ~151–190) so it delegates, and add the new items. Also change `render_deep`'s signature to accept `verify_dropped: Option<usize>`.

Replace `finalize_deep_review` with:

```rust
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
```

Change the existing `fn render_deep(...)` signature to add the trailing param and emit the note. Its current header-building block (the `if is_error {..} else if merged.is_empty() {..} else {..}` at lines ~200–224) becomes:

```rust
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
    // ... the rest (failed line + the per-finding loop) is UNCHANGED ...
```

Keep the remainder of `render_deep` (the `if !failed.is_empty()` line and the `for (i, m) in merged.iter()...` loop) exactly as-is.

Note: `verify_dropped = None` yields an empty `verify_note`, so `finalize_deep_review`'s output is byte-identical to Phase 1 — the `finalize_output_is_unchanged_after_the_split` test asserts this.

Then add the verify machinery at the end of the module body (before `#[cfg(test)]`):

```rust
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
```

In `lib.rs`, extend the fanout re-export to include the new public items:

```rust
pub use fanout::{
    dimension_coverage, merge_deep_findings, render_deep_result, render_verify_task, run_verify,
    ReviewDimension, DimensionOutcome, MergedFinding, REVIEW_DIMENSIONS, VERIFY_CONCURRENCY,
    VERIFY_LENS,
};
```
(Keep whatever fanout items were already re-exported; add these. If `finalize_deep_review`/`run_deep_review` were exported, leave them.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p atomcode-review --lib fanout` then `cargo test -p atomcode-review`.
Expected: PASS, including the pre-existing `finalize_*` / `run_deep_review_*` tests (unchanged output).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/fanout.rs crates/atomcode-review/src/lib.rs
git commit -m "feat(review): split deep finalize + add verify lens/runner (phase 2 scaffolding)"
```

---

### Task 2: Wire `deep+verify` into `code_review` execute (review_tool.rs)

**Files:**
- Modify: `crates/atomcode-review/src/review_tool.rs`
- Test: in `review_tool.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `fanout::{merge_deep_findings, dimension_coverage, render_deep_result, render_verify_task, run_verify, VERIFY_LENS, VERIFY_CONCURRENCY}`, plus the already-imported `run_deep_review`, `DimensionOutcome`, `REVIEW_DIMENSIONS`.
- Produces: `code_review` accepting `{"depth":"deep+verify"}`; `Args::wants_verify()`; schema enum grows.

- [ ] **Step 1: Write the failing tests**

Add to `review_tool.rs` `mod tests`:

```rust
    #[test]
    fn args_parse_deep_verify_depth() {
        let v: Args = serde_json::from_str(r#"{"depth":"deep+verify"}"#).unwrap();
        assert!(v.is_deep(), "deep+verify still counts as deep (fans out)");
        assert!(v.wants_verify());
        let d: Args = serde_json::from_str(r#"{"depth":"deep"}"#).unwrap();
        assert!(d.is_deep() && !d.wants_verify());
        let s: Args = serde_json::from_str("{}").unwrap();
        assert!(!s.is_deep() && !s.wants_verify());
    }

    #[tokio::test]
    async fn deep_verify_keeps_a_confirmed_finding() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = repo_with_working_tree_change();
        let provider: SharedReviewProvider =
            Arc::new(RwLock::new(Some(Arc::new(ScriptedReviewProvider))));
        let tool = ReviewTool::new(
            provider,
            ReviewToolConfig { model: "mock-model".into(), ..Default::default() },
        );
        let ctx = ToolContext {
            working_dir: dir.path().to_path_buf(),
            cancel: Default::default(),
            progress: ProgressSink::noop(),
            requester: None,
        };

        let res = tool.execute(r#"{"depth":"deep+verify"}"#, &ctx).await;

        // 4 dimensions report the same finding → merged to 1; each finding's
        // verify agent (ScriptedReviewProvider) re-reports it → kept, dropped 0.
        assert!(!res.is_error, "deep+verify should succeed: {}", res.content);
        assert!(res.content.contains("Deep review"), "deep header: {}", res.content);
        assert!(res.content.contains("verify dropped 0"), "verify note, nothing culled: {}", res.content);
        assert!(res.content.contains("1 finding"), "the confirmed finding survives: {}", res.content);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-review --lib args_parse_deep_verify_depth deep_verify_keeps_a_confirmed_finding`
Expected: FAIL to compile — `wants_verify` missing / `deep+verify` not handled.

- [ ] **Step 3: Write the implementation**

(a) Update `is_deep` and add `wants_verify` in `impl Args` (replace the existing `is_deep`):

```rust
    fn is_deep(&self) -> bool {
        self.depth
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("deep") || d.eq_ignore_ascii_case("deep+verify"))
            .unwrap_or(false)
    }

    fn wants_verify(&self) -> bool {
        self.depth
            .as_deref()
            .map(|d| d.eq_ignore_ascii_case("deep+verify"))
            .unwrap_or(false)
    }
```

(b) Update the imports (the `use crate::fanout::...` line ~36) to add the new items:

```rust
use crate::fanout::{
    dimension_coverage, merge_deep_findings, render_deep_result, render_verify_task, run_deep_review,
    run_verify, DimensionOutcome, REVIEW_DIMENSIONS, VERIFY_CONCURRENCY, VERIFY_LENS,
};
```
(Drop `finalize_deep_review` from the import if it is no longer referenced — the deep branch now uses `merge_deep_findings`/`render_deep_result`. Leave it imported only if still used elsewhere.)

(c) Update the schema `depth` entry (line ~439) to the new enum + description:

```rust
                "depth": { "type": "string", "enum": ["single", "deep", "deep+verify"], "description": "Review depth. `deep` fans out one reviewer per concern dimension (correctness/security/performance/tests) and merges findings; `deep+verify` additionally runs one verify pass per finding to cull false positives; omit for the default single reviewer." }
```

(d) Replace the deep-path tail (currently the two lines `let (is_error, content) = finalize_deep_review(&outcomes, files.len(), &files); if is_error { err(content) } else { ok(content) }` at lines ~567–568) with the merge → optional-verify → render sequence:

```rust
        // Merge the fan-out outcomes; optionally cull false positives with a
        // single verify pass per surviving finding.
        let (mut merged, deduped) = merge_deep_findings(&outcomes, &files);
        let (completed, failed) = dimension_coverage(&outcomes);
        let mut verify_dropped = None;
        if a.wants_verify() && !merged.is_empty() {
            // One verify agent per finding, capped. Keep a finding when its
            // verify agent re-reports it (or fails open on error/cancel).
            let inputs: Vec<String> = merged
                .iter()
                .map(|m| render_verify_task(&m.finding, &rules, &annotated))
                .collect();
            let keep = run_verify(merged.len(), VERIFY_CONCURRENCY, |i| {
                let provider = provider.clone();
                let vtask = inputs[i].clone();
                let mut cfg = make_cfg();
                let cancel = ctx.cancel.clone();
                async move {
                    cfg = cfg.with_persona_append(VERIFY_LENS);
                    let (agent, report) = build_review_agent_with(&cfg, provider);
                    let (stop, run_error) = tokio::select! {
                        _ = cancel.cancelled() => (StopReason::Cancelled, Some("cancelled by user".to_string())),
                        outcome = agent.run_to_completion(vtask, AutoRespond::AllowAll) => {
                            (outcome.stop, outcome.error)
                        }
                    };
                    // Fail-open: keep on error/cancel; else keep iff the verifier re-reported.
                    let clean = stop == StopReason::Stopped && run_error.is_none();
                    let kept = if clean { !report.findings().is_empty() } else { true };
                    (i, kept)
                }
            })
            .await;
            let before = merged.len();
            let mut mask = keep.into_iter();
            merged.retain(|_| mask.next().unwrap_or(true));
            verify_dropped = Some(before - merged.len());
        }
        let (is_error, content) =
            render_deep_result(&merged, files.len(), &completed, &failed, deduped, verify_dropped);
        if is_error { err(content) } else { ok(content) }
```

Note on the closure: `render_verify_task` and `make_cfg()` are called synchronously (producing owned `String`/`cfg`) BEFORE `async move`, and `provider`/`cancel` are cloned — so each verify future is `Send + 'static`, exactly like the dimension closure. `inputs[i].clone()` reads the local `inputs` (kept alive across the `.await`). Do NOT weaken `run_verify`'s bounds; if the borrow checker fights the closure, mirror the dimension closure's structure. If you cannot satisfy `Send + 'static` after honest effort, STOP and report BLOCKED with the exact error.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p atomcode-review --lib args_parse_deep_verify_depth deep_verify_keeps_a_confirmed_finding` then `cargo test -p atomcode-review`.
Expected: PASS; all Phase 1 tests (incl. `deep_review_fans_out_and_dedups_across_dimensions`, single-path tests) stay green.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-review/src/review_tool.rs
git commit -m "feat(review): code_review deep+verify runs one verify pass per finding"
```

---

### Task 3: `/review deep+verify` command mapping (commands.rs)

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs` (`review_prompt`)
- Test: in `commands.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `/review deep+verify [scope]` synthesizes a tool call carrying `"depth":"deep+verify"`; `deep` and plain scopes unchanged.

- [ ] **Step 1: Write the failing test**

Add to `commands.rs` `mod tests`:

```rust
    #[test]
    fn review_prompt_deep_verify_sets_depth_and_keeps_scope() {
        let wt = review_prompt("deep+verify");
        assert!(wt.contains(r#""scope":{"kind":"working_tree"}"#), "{wt}");
        assert!(wt.contains(r#""depth":"deep+verify""#), "{wt}");

        let st = review_prompt("deep+verify staged");
        assert!(st.contains(r#""scope":{"kind":"staged"}"#), "{st}");
        assert!(st.contains(r#""depth":"deep+verify""#), "{st}");

        let rng = review_prompt("deep+verify main");
        assert!(rng.contains(r#""scope":{"kind":"range","base":"main","head":"HEAD"}"#), "{rng}");
        assert!(rng.contains(r#""depth":"deep+verify""#), "{rng}");

        // Plain `deep` still maps to depth "deep" (not deep+verify).
        let d = review_prompt("deep");
        assert!(d.contains(r#""depth":"deep""#) && !d.contains("deep+verify"), "{d}");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix --lib review_prompt_deep_verify_sets_depth`
Expected: FAIL — `deep+verify` is currently parsed as a git ref, emitting a range scope with no depth.

- [ ] **Step 3: Write the implementation**

Replace the leading-keyword parse + args-object build inside `review_prompt` (the current `let (deep, scope) = match arg.strip_prefix("deep") {...};` block and the `let args = if deep {...} else {...};` block) with a depth-aware version that checks `deep+verify` before `deep`:

```rust
    // A leading `deep+verify` or `deep` keyword (alone or before a scope) sets depth.
    let (depth, scope): (Option<&str>, &str) = if let Some(rest) = arg
        .strip_prefix("deep+verify")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    {
        (Some("deep+verify"), rest.trim())
    } else if let Some(rest) = arg
        .strip_prefix("deep")
        .filter(|r| r.is_empty() || r.starts_with(char::is_whitespace))
    {
        (Some("deep"), rest.trim())
    } else {
        (None, arg)
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
    let args = match depth {
        Some(d) => format!(r#"{{"scope":{scope_json},"depth":"{d}"}}"#),
        None => format!(r#"{{"scope":{scope_json}}}"#),
    };
```

Keep the closing `format!("Review the requested changes: call the `code_review` tool with {args}, ...")` line unchanged. This preserves every substring the existing tests assert (plain scopes → no `depth`; `deep` → `"depth":"deep"`; range JSON escaping via `serde_json::to_string`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p atomcode-tuix --lib review_prompt`
Expected: PASS — the new deep+verify test plus the existing `review_prompt_uses_explicit_tool_scopes`, `review_prompt_json_escapes_the_base_ref`, and `review_prompt_deep_adds_depth_and_keeps_scope` all green.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(tuix): /review deep+verify maps to depth deep+verify"
```

---

### Task 4: Full-suite regression + doc note

**Files:**
- Modify: `crates/atomcode-review/src/review_tool.rs` (extend the module header note)

- [ ] **Step 1: Extend the doc note**

In the `review_tool.rs` module header, update the deep-mode note to mention verify:

```rust
//! Deep mode: `{"depth":"deep"}` fans out one read-only reviewer per concern
//! dimension (see `crate::fanout`) and merges/dedups their findings;
//! `{"depth":"deep+verify"}` additionally runs one verify pass per finding to
//! cull false positives (single vote, biased toward keep). The default
//! single-reviewer path is unchanged.
```

- [ ] **Step 2: Run the suites**

```bash
cargo test -p atomcode-review
cargo test -p atomcode-tuix --lib
cargo build -p atomcode-review -p atomcode-tuix
```
Expected: all green, zero new warnings.

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-review/src/review_tool.rs
git commit -m "docs(review): note deep+verify pass in code_review header"
```

---

## Self-Review

**Spec coverage:**
- `depth:"deep+verify"` trigger + `is_deep` true for both + `wants_verify` → Task 2 (Args), Task 3 (command).
- Verify between merge and render → Task 2 (deep branch: merge_deep_findings → run_verify → render_deep_result).
- Verify agent reuses `build_review_agent_with` + `report_finding`; keep = re-reported; drop = reported nothing; fail-open on error/cancel → Task 2 closure (`clean` gate; `unwrap_or(true)`), Task 1 `run_verify` default-true.
- Single vote biased toward keep → `VERIFY_LENS` wording (Task 1) + fail-open logic (Task 2).
- Bounded concurrency via JoinSet, no `futures` → Task 1 `run_verify` (`VERIFY_CONCURRENCY`).
- Reporting "verify dropped K" → Task 1 `render_deep`/`render_deep_result` verify_note.
- No-verify path byte-identical → Task 1 `finalize_deep_review` delegates with `None`; `finalize_output_is_unchanged_after_the_split` asserts it.
- single/deep unchanged → Task 2 keeps the `!is_deep()` block and the deep fan-out untouched except its render tail.

**Placeholder scan:** none — all steps carry concrete code.

**Type consistency:** `merge_deep_findings`/`dimension_coverage`/`render_deep_result`/`render_verify_task`/`run_verify`/`VERIFY_LENS`/`VERIFY_CONCURRENCY` are defined in Task 1 and consumed with identical signatures in Task 2. `run_verify` returns `Vec<bool>` consumed as a keep-mask via `retain`. `wants_verify`/`is_deep` defined in Task 2 and used there. The verify closure mirrors the Phase-1 dimension closure's `Send + 'static` structure.
