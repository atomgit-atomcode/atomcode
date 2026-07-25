# Provider Retry Consolidation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Consolidate HTTP 429 retry ownership in the kernel so transient RPM/TPM or gateway throttling retries with cancellable bounded backoff, without multiplying provider retries or replaying partial streamed output.

**Architecture:** Retry ownership is selected per provider call. Direct consumers (compaction, vision, title, evaluation) retain bounded provider-owned 429 retries, while the kernel turn loop marks its calls kernel-owned so the first 429 is surfaced with structured status, code, body, and `Retry-After`. The kernel owns that call's incident budget and chooses between server-directed waiting, jittered fallback exponential backoff, confirmed long CodingPlan quota pause, terminal billing/balance errors, and a five-wait fuse. Mid-stream 429 retries only before any user-visible model content has been emitted; visible partial output is persisted before the clean rate-limited terminal.

**Tech Stack:** Rust, Tokio, reqwest, atomcode-kernel lifecycle hooks, atomcode-capabilities provider adapters, atomcode-coding CodingPlan hook.

---

### Task 1: Separate provider retryability from provider-owned retry execution

**Files:**
- Modify: `crates/atomcode-capabilities/src/provider/retry.rs`
- Modify: `crates/atomcode-capabilities/src/provider/openai_compat.rs`
- Modify: `crates/atomcode-capabilities/src/provider/anthropic.rs`
- Modify: `crates/atomcode-capabilities/src/provider/ollama.rs`

**Steps:**

1. Add tests proving 429 remains retryable and its OPEN-loop behavior follows the per-call owner.
2. Add a runtime-only retry owner to `ChatOptions`; direct calls default to provider-owned, while the kernel turn loop overrides it to kernel-owned.
3. Switch all three provider OPEN loops to the ownership-aware helper.
4. Keep final 429 errors marked `retryable`, with `Retry-After` and provider error data intact for the kernel.
5. Run `cargo test -p atomcode-capabilities provider::retry --lib`.

### Task 2: Give unknown transient 429s bounded kernel backoff

**Files:**
- Modify: `crates/atomcode-kernel/src/hook.rs`
- Modify: `crates/atomcode-kernel/src/agent.rs`
- Modify: `crates/atomcode-kernel/src/testkit.rs`
- Modify: `crates/atomcode-kernel/tests/rate_limit.rs`
- Modify: `crates/atomcode-coding/src/rate_limit.rs`

**Steps:**

1. Add policy tests for missing `Retry-After`: 3, 6, 12, 24, 48-second bases by incident attempt, with ±25% jitter.
2. Extend `RateLimitHint` with the one-based incident attempt.
3. Make `RateLimitDecision::from_hint` honor `Retry-After <= 120s`, pause for longer server-directed waits, stop known billing/balance errors immediately, and use bounded jittered fallback backoff when the header is absent.
4. Populate the attempt from the turn-owned rate-limit counter in both OPEN and mid-stream paths.
5. Update CodingPlan and testkit constructors.
6. Run kernel and coding rate-limit tests.

### Task 3: Prevent partial-stream replay

**Files:**
- Modify: `crates/atomcode-kernel/src/agent.rs`
- Modify: `crates/atomcode-kernel/tests/rate_limit.rs`

**Steps:**

1. Add a failing test where a text delta is followed by a 429 and verify no second provider request occurs.
2. Allow mid-stream 429 auto-retry only when no text, reasoning, reasoning signature, or tool call has been emitted.
3. Persist already-visible partial output, then emit a clean `RateLimited` terminal with the server reason.
4. Preserve cancellable waiting and the single terminal invariant.
5. Run `cargo test -p atomcode-kernel --test rate_limit`.

### Task 4: Cross-layer verification

**Files:**
- Review: all modified files

**Steps:**

1. Run focused provider, kernel, and coding tests.
2. Run `cargo test -p atomcode-capabilities -p atomcode-kernel -p atomcode-coding --lib`.
3. Run `cargo check --workspace --all-targets`.
4. Run `git diff --check` and inspect the final diff for retry multiplication, cancellation, and terminal-state regressions.
