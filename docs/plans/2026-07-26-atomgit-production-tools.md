# AtomGit Production Tools Implementation Plan

> **For Codex:** Execute this plan task-by-task in the current workspace while preserving unrelated user changes.

**Goal:** Expose the built-in AtomGit repository, pull-request, and issue tools to production coding runtimes and steer the model away from raw credential-bearing `curl` calls.

**Architecture:** Keep AtomGit REST ownership in `atomcode-capabilities`. Add one coding-layer registration helper shared by the minimal agent builder and the production `prepare → assemble` path, so both publish the same tool catalog. Add prompt guidance only when the `atomgit` feature is compiled, and verify the production `CodingParts` catalog rather than only the low-level registry.

**Tech Stack:** Rust, atomcode-kernel tool registry, atomcode-coding two-phase runtime assembly, Cargo tests.

---

### Task 1: Reproduce the production catalog gap

**Files:**
- Modify: `crates/atomcode-coding/src/parts.rs`

1. Add a feature-gated unit test that prepares production `CodingParts`.
2. Assert that `atomgit_repo`, `atomgit_pr`, and `atomgit_issue` are selected.
3. Run the focused test and confirm that it fails because the names are absent.

### Task 2: Share AtomGit tool registration

**Files:**
- Modify: `crates/atomcode-coding/src/assemble.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`

1. Extract a feature-gated helper that creates the AtomGit client, registers all three tools, and appends their names.
2. Fail explicitly if client construction fails; do not silently publish an incomplete catalog.
3. Reuse the helper from both the minimal builder and production preparation.
4. Run the focused production catalog test.

### Task 3: Prefer dedicated AtomGit tools

**Files:**
- Modify: `crates/atomcode-coding/src/persona.rs`

1. Add feature-gated prompt guidance to use `atomgit_repo`, `atomgit_pr`, and `atomgit_issue` instead of reading auth files or constructing raw API `curl` commands.
2. Add a persona test covering the guidance.
3. Run the focused persona test.

### Task 4: Verify the affected crate

1. Run formatting checks for changed Rust files.
2. Run `cargo test -p atomcode-coding --features atomgit`.
3. Inspect the final diff and confirm unrelated dirty files remain untouched.
