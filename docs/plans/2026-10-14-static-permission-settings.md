# Static permission settings implementation plan

**Goal:** Load Claude Code shaped JSON permission lists for users and projects through the existing coding runtime permission gate.

**Baseline:** `release/v5.2.4`, `1a2dfce87b9ca8d7ed2fc685a970276d478e50a1`, clean worktree. Static TOML permissions already exist (introduced in `cf08afad0`); JSON loading and project merging do not. Hook commands run directly after plugin trust, independently of Bash tool approval.

**Architecture:** The runtime remains the lifecycle owner. Resolve settings at `tree_inputs`, once per mount, against the runtime working directory and supplied `ProductDirs`. Keep configured TOML rules separate from the per-mount JSON projection, so changing directory never retains another project's grants. Reuse `PermissionRules` for matching and gate ordering. The tool catalog uses the same parsed rules to omit only tools denied for every argument, including tools published later by MCP. Invalid or inaccessible settings and malformed permission rules fail the candidate mount; existing runtime reconfiguration handles rollback.

**Files:** User product tree `settings.json` and `settings.local.json`; current working directory `.claude/settings.json`, `.claude/settings.local.json`, and the supplied product project directory's same two files. No ancestor traversal or global Claude home lookup. Merge allow and deny lists additively with deny taking precedence across every source. Unrelated JSON fields are ignored; present permission fields must have the expected types. Missing files are normal. No settings writer, hook authorization changes, or second runtime owner.

**Tech stack:** Rust, existing serde_json, capabilities PermissionRules, harness ToolPolicy, coding runtime mount.

1. Add parsed-rule merging and unconditional tool-deny queries in `capabilities/src/tools/permission_rules.rs`; test aliases, whole-tool versus argument rules, and deny precedence.
2. Add `coding/src/permission_settings.rs` with strict, path-qualified JSON loading and source tracking; test every scope, unrelated fields, custom product directories, and invalid/inaccessible files.
3. Resolve permissions at runtime tree construction. Give the tool catalog the effective rules without overwriting include/exclude or session switches; register settings self-knowledge from the row that enforces them.
4. Add end-to-end tests for JSON command preapproval, blocked command side effects, hidden Web tools, late tool registration, fresh/resume/provider rebuild/directory switch, and failed reconfiguration retaining the previous policy.
5. Document file scopes, additive deny precedence, reload timing, unconditional versus command denies, and independent plugin hook trust in `docs/config.example.toml` and `docs/hooks.md`.
6. Run focused tests, then affected crate suites with nextest; run `cargo fmt --all`, `cargo fmt --all -- --check`, `bash gates/product-dir.sh`, and `bash gates/compile.sh`. Check the final diff and tracked Cargo.lock (skip-worktree is already set).

## Validation

- `cargo nextest run -p atomcode-coding`: 998 passed, 9 skipped.
- `cargo nextest run -p atomcode-harness`: 242 passed.
- `cargo nextest run -p atomcode-capabilities`: 1050 passed (default features).
- `bash gates/compile.sh`: workspace/all-targets, feature-gated targets and both negative controls passed.
- `cargo fmt --all -- --check`, `bash gates/product-dir.sh` (baseline 0), and `git diff --check`: passed.
- Tracked Cargo.lock unchanged; its existing skip-worktree flag preserved.

The new lifecycle and catalog tests use paused time. The real-shell criterion
uses wall time because auto-advancing Tokio's clock expires the Bash timeout
before the OS subprocess completes. No real provider, USB HID hardware, or
vibekeys-plugin installation was used for validation.

## Pre-commit audit

The audit corrected two boundary cases: a present dangling settings symlink
must fail closed rather than count as a missing file, and allow / argument deny
rules must not report the tool catalog as restricted. Non-file settings sources
are rejected before reading; symlinks to regular settings files remain supported.
Regression criteria cover both cases. Final verification uses an isolated checkout
containing only this permission change, excluding concurrent startup and TUI edits.
The audit also corrected the reload documentation: a model-only provider patch
does not rebuild the tool tree, so it keeps current rules. The lifecycle criterion
now edits the file, proves the model patch retains the active policy, then proves
a full config reprepare reads the new policy.
Final verification ran sequentially with isolated source and build directories,
on top of `6b59c18a24e96287b426235e4eb00f177efefd6b`: all 2290 selected tests,
format/product-directory checks and the compile gate passed. Golden fixtures and
the differential baseline were unchanged.
