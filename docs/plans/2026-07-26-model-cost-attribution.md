# Model Cost Attribution Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make `/cost` attribute token usage and estimated cost to the provider/model that produced it, without relabeling historical usage after `/model`.

**Architecture:** `CodingRuntime` already rebuilds its assembled agent for provider/model changes. Each generation will construct the native `SnapshotHook` with that generation's stable provider/model identity and optional pricing snapshot. The hook will accumulate every model response in the turn and persist additive model-usage records in native session metadata. TUI, daemon, and remote `/cost` will consume one session aggregation model; live TUI counters remain presentation-only.

**Tech Stack:** Rust, serde-compatible native session metadata, atomcode-kernel lifecycle hooks, atomcode-coding runtime assembly, TUI/daemon command projections.

---

### Task 1: Native usage schema and aggregation

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/manager.rs`
- Modify: `crates/atomcode-capabilities/src/session/mod.rs`
- Test: `crates/atomcode-capabilities/src/session/manager.rs`

**Steps:**
1. Add failing tests for provider/model grouping, same model names under different providers, unknown pricing, and legacy unattributed totals.
2. Add serde-defaulted token, pricing snapshot, per-model usage, and report types.
3. Implement aggregation by `(provider_id, model_id)` while retaining legacy `TurnStat.total_tokens` as unattributed data only when no detailed records exist.
4. Run the focused session tests.

### Task 2: Runtime-owned attribution

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/snapshot.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`
- Test: `crates/atomcode-capabilities/src/session/snapshot.rs`

**Steps:**
1. Add a failing hook test with two model responses in one turn.
2. Give `SnapshotHook` an immutable generation attribution configured by `atomcode-coding`.
3. Accumulate prompt, completion, and cached tokens for every response.
4. Persist detailed usage at `turn_complete`; keep old constructors unattributed for compatibility tests.
5. Verify model reload naturally rebuilds the hook with the new runtime config.

### Task 3: Provider pricing configuration

**Files:**
- Modify: `crates/atomcode-config/src/config/provider.rs`
- Modify: `crates/atomcode-coding/src/config.rs`
- Modify provider API projections only where compilation requires it.
- Test: `crates/atomcode-config/src/config/provider.rs`

**Steps:**
1. Add tests for omitted, explicit-free, and configured per-million prices.
2. Add an optional provider pricing object; omitted means unknown rather than zero.
3. Resolve the immutable pricing snapshot into `CodingRuntimeConfig`.
4. Do not introduce a remote model-price service in this change.

### Task 4: Unified `/cost` projection

**Files:**
- Modify: `crates/atomcode-daemon/src/commands.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`
- Modify: `crates/atomcode-tuix/src/session.rs`
- Modify: `crates/atomcode-tuix/src/i18n` files as required.
- Test: command and rendering unit tests in the touched crates.

**Steps:**
1. Add failing tests for A usage followed by a switch to unused B.
2. Replace current-model repricing with the native session aggregation.
3. Render per-provider/model token lines, estimated cost only for known/free pricing, and a legacy “unattributed” section.
4. Make TUI, daemon, and remote command projections use the same report semantics.
5. Remove the unknown-model `$1/$3` fallback from the `/cost` path.

### Task 5: Compatibility and verification

**Files:**
- Review all modified files and relevant fixtures.

**Steps:**
1. Run affected crate tests after each logical unit.
2. Run cross-crate tests/checks for config, capabilities, coding, daemon, and tuix.
3. Verify old metadata deserializes and remains unattributed.
4. Audit provider reload, resume, undo, compaction, cancel, and generation boundaries.
5. Inspect the final diff for unrelated changes and document any unverified true-device behavior.
