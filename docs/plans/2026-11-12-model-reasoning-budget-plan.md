# Model Reasoning Budget Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Support per-model continuous reasoning budgets and archive incomplete evaluation candidates with explicit diagnostics.

**Architecture:** Keep configuration resolution in atomcode-config and enforcement in the existing watchdog. Route the selected budget through coding runtime and delegated model construction. Extend DeepSWE evidence capture without changing score eligibility or active runs.

**Tech Stack:** Rust, serde/TOML, nextest, Python 3.11 unittest.

### Task 1: Model configuration and runtime

- Add optional `reasoning_timeout_secs` to legacy providers, model profiles, and resolved models in `crates/atomcode-config/src/config/provider.rs`.
- Preserve it through projection and resolution in `crates/atomcode-config/src/config/mod.rs`.
- Apply valid environment override, then model value, then 300 seconds through `crates/atomcode-coding/src/config.rs`; zero disables.
- Propagate the policy through daemon and delegated model construction; avoid provider-wire changes.
- Verify model selection, legacy projection, zero, invalid values, environment precedence and mounted watchdog limits.

### Task 2: Evaluation evidence

- Update `deep-swe/scripts/atomcode_eval.py` in the benchmark toolkit and the live DeepSWE checkout for future launches only.
- Archive error code, actual reported budget when available, execution limits, candidate hash, and incomplete status.
- Keep incomplete patches outside official `model.patch`; never convert interrupted candidates into normal passes.
- Test locally with a fake executable and temporary Git repositories, including timeout and successful completion.

### Task 3: Verification

- Run focused config/runtime/watchdog tests and Python evidence tests.
- Run formatting, diff checks and `bash gates/compile.sh`.
- Document TOML examples and distinguish saved evidence from automatic recovery.
- Leave active frozen-binary validation and its environment unchanged. Do not commit unrelated work.

## Validation results

- Config/coding unit and mount tests: 1,076 passed, 9 skipped.
- Benchmark evidence tests: 4 passed, including preservation when diagnostic writing fails.
- Workspace compile gate, feature compilation, and its negative controls passed.
- Formatting and diff checks passed for this change.
- Final review reconciled two obsolete recovery expectations with the explicit
  not-dispatched proof. The tests continue to require unknown tool execution
  status to fail closed and preserve the original reasoning effort.
- All 70 reasoning-deadline and coding recovery tests passed on the combined
  working tree. No release binary or user model configuration was replaced.
  The running pilot retains its frozen binary and zero override.
