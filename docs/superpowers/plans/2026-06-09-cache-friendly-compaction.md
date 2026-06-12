# Cache-Friendly History Compaction — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop prompt-prefix-cache breaks on near-1M-token deepseek-v4-flash sessions by replacing the ephemeral per-render `microcompact` with a committed, idempotent, monotonic collapse of old tool results.

**Architecture:** Today `microcompact` (render.rs) re-derives which old `ToolResult`s to stub on every render against a throwaway Vec, never persisting — so the rendered prefix drifts byte-for-byte between turns and the provider cache collapses. We move the stub into a committed step (`collapse_committed`) that mutates `conv.messages` once, idempotently, before the actual-send render in `turn/runner.rs`. Old tool results, once stubbed, stay byte-identical forever (monotonic), so the prefix is append-only. The active turn (everything after the last `Role::User`) stays full; `read_file` is exempt on this normal path. The independent 80% `FINAL BYTE CEILING` and emergency Tier-3 truncate (the real overflow guards) are untouched, so no litellm context-overflow regression.

**Tech Stack:** Rust, `cargo` workspace, crate `atomcode-core`. Tests are `#[test]` fns in the same files (`mod tests`).

**Spec:** `docs/superpowers/specs/2026-06-09-cache-friendly-compaction-design.md`

**Worktree / branch:** `/Users/lichao/project/gitcode/ai/atomcode-v4.25.1`, branch `fix/cache-friendly-compaction`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/atomcode-core/src/ctx/render.rs` | Rendering + compaction policy | Add `exempt_read_file` param to `compact_old_tool_results_in_place`; add `collapse_committed` free fn; delete `microcompact` fn + its call inside `build_messages`; migrate tests |
| `crates/atomcode-core/src/turn/runner.rs` | Per-turn provider render/send | Call `collapse_committed(&mut conversation, context_window)` immediately before the actual-send `build_messages` |
| `crates/atomcode-core/src/agent/mod.rs` | Agent loop, emergency compaction | Update emergency Tier-1 call site to pass `exempt_read_file = false` (behavior unchanged) |
| `crates/atomcode-core/src/agent/compression.rs` | Compression helpers | Update `compact_old_tool_results_in_place` call site to pass `false` |

All work happens in the worktree above. Run all commands from `/Users/lichao/project/gitcode/ai/atomcode-v4.25.1`.

---

## Task 1: Add `exempt_read_file` to `compact_old_tool_results_in_place`

Adds a flag so the normal path can skip `read_file` (avoids "伪自信" re-edit loop) while the emergency path keeps collapsing it under real budget pressure.

**Files:**
- Modify: `crates/atomcode-core/src/ctx/render.rs` (fn at ~869–900, signature line 869)
- Modify call sites: `crates/atomcode-core/src/agent/mod.rs:2936`, `crates/atomcode-core/src/agent/compression.rs:282`, and every other caller the compiler flags
- Test: `crates/atomcode-core/src/ctx/render.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `render.rs` (near the other `collapse_*` tests):

```rust
    #[test]
    fn compact_old_exempts_read_file_when_flagged() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "r".into(), name: "read_file".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "r".into(),
            output: format!("L1\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1"); // active turn → kept full

        compact_old_tool_results_in_place(&mut conv, 1, true);

        let get = |cid: &str, conv: &Conversation| {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        assert!(!get("r", &conv).starts_with('['), "read_file must stay full when exempt");
        assert!(get("b", &conv).starts_with("[bash "), "bash must be stubbed");
    }

    #[test]
    fn compact_old_stubs_read_file_when_not_exempt() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "r".into(), name: "read_file".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "r".into(),
            output: format!("L1\n{}", "x".repeat(2_000)),
            success: true,
        });
        conv.add_user_message("t1");

        compact_old_tool_results_in_place(&mut conv, 1, false);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "r" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert!(out.starts_with("[read_file "), "emergency path must still stub read_file");
    }
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test -p atomcode-core compact_old_exempts_read_file_when_flagged 2>&1 | tail -20`
Expected: compile error — `compact_old_tool_results_in_place` takes 2 args, 3 supplied.

- [ ] **Step 3: Add the parameter and the skip**

In `render.rs`, change the signature (line ~869) from:

```rust
pub(crate) fn compact_old_tool_results_in_place(
    conv: &mut crate::conversation::Conversation,
    keep_recent_turns: usize,
) {
```

to:

```rust
pub(crate) fn compact_old_tool_results_in_place(
    conv: &mut crate::conversation::Conversation,
    keep_recent_turns: usize,
    exempt_read_file: bool,
) {
```

Inside the loop, immediately after `let tool_name = call_id_to_tool.get(&tr.call_id).map(|s| s.as_str()).unwrap_or("tool");` and BEFORE `let summary = build_compact_stub(...)`, insert:

```rust
        if exempt_read_file && tool_name == "read_file" {
            continue;
        }
```

- [ ] **Step 4: Fix all existing call sites (compiler-guided)**

Run: `cargo build -p atomcode-core 2>&1 | grep -E 'compact_old_tool_results_in_place|error\[' | head`
Every call site errors on arity. Add a third argument `false` to **every** existing call (all of them must preserve today's behavior — only the new `collapse_committed` in Task 2 passes `true`):
- `agent/mod.rs:2936` → `compact_old_tool_results_in_place(&mut self.conversation, 3, false)`
- `agent/compression.rs:282` → `..., 3, false)`
- Any call inside `render.rs`/`agent/mod.rs` test modules (e.g. `compact_old_tool_results_in_place(&mut conv, 3, false)`, `..., 2, false)`, `..., 1, false)`) → append `, false`.

Repeat `cargo build -p atomcode-core` until it compiles.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p atomcode-core compact_old 2>&1 | tail -20`
Expected: `compact_old_exempts_read_file_when_flagged` and `compact_old_stubs_read_file_when_not_exempt` PASS, plus the pre-existing `collapse_*` tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-core/src/ctx/render.rs crates/atomcode-core/src/agent/mod.rs crates/atomcode-core/src/agent/compression.rs
git commit -m "feat(ctx): add exempt_read_file flag to compact_old_tool_results_in_place"
```

---

## Task 2: Add the committed `collapse_committed` collapse

The normal-path entry point: threshold-gated, keeps the active turn full, exempts read_file, commits to `conv.messages`.

**Files:**
- Modify: `crates/atomcode-core/src/ctx/render.rs` (add free fn near `compact_old_tool_results_in_place`)
- Test: `crates/atomcode-core/src/ctx/render.rs` (`mod tests`)

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn collapse_committed_noop_below_threshold() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(1_000)),
            success: true,
        });
        conv.add_user_message("t1");

        // budget 1_000_000 → threshold 2.8M chars; ~1K payload → no-op.
        collapse_committed(&mut conv, 1_000_000);

        let out = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert!(!out.starts_with('['), "below threshold must stay full (append-only)");
    }

    #[test]
    fn collapse_committed_stubs_old_keeps_active_and_exempts_read_file() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        // 3 old turns: each a big read_file + big bash; then an active turn.
        for n in 0..3 {
            conv.add_user_message(&format!("t{n}"));
            let r = format!("r{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: r.clone(), name: "read_file".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: r,
                output: format!("L1\n{}", "x".repeat(6_000)),
                success: true,
            });
            let b = format!("b{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: b.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: b,
                output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
                success: true,
            });
        }
        // active turn (kept full): a bash that must NOT be stubbed.
        conv.add_user_message("active");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "ba".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "ba".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
            success: true,
        });

        // budget 8_000 → threshold 22_400 chars; payload ~42K → fires.
        collapse_committed(&mut conv, 8_000);

        let get = |cid: &str, conv: &Conversation| {
            conv.messages.iter().find_map(|m| match &m.content {
                MessageContent::ToolResult(r) if r.call_id == cid => Some(r.output.clone()),
                _ => None,
            }).unwrap()
        };
        // old bash → stubbed; old read_file → exempt (full); active bash → full.
        assert!(get("b0", &conv).starts_with("[bash "), "old bash must be stubbed");
        assert!(!get("r0", &conv).starts_with('['), "old read_file must stay full (exempt)");
        assert!(!get("ba", &conv).starts_with('['), "active-turn bash must stay full");
    }

    #[test]
    fn collapse_committed_is_idempotent() {
        use crate::tool::{ToolCall, ToolResult};
        let mut conv = Conversation::new();
        conv.add_user_message("t0");
        conv.add_assistant_tool_calls(
            None,
            vec![ToolCall { id: "b".into(), name: "bash".into(), arguments: "{}".into() }],
            None,
        );
        conv.add_tool_result(ToolResult {
            call_id: "b".into(),
            output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(30_000)),
            success: true,
        });
        conv.add_user_message("t1");

        collapse_committed(&mut conv, 8_000);
        let after_first = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        collapse_committed(&mut conv, 8_000);
        let after_second = conv.messages.iter().find_map(|m| match &m.content {
            MessageContent::ToolResult(r) if r.call_id == "b" => Some(r.output.clone()),
            _ => None,
        }).unwrap();
        assert_eq!(after_first, after_second, "re-running must not re-stub (idempotent)");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-core collapse_committed 2>&1 | tail -20`
Expected: compile error — `collapse_committed` not found.

- [ ] **Step 3: Implement `collapse_committed`**

In `render.rs`, immediately after the `compact_old_tool_results_in_place` fn (after its closing `}` at ~line 900), add:

```rust
/// Normal-path committed collapse (cache-friendly replacement for the
/// removed ephemeral `microcompact`). Threshold-gated by the same
/// `70% × budget × 4` char trigger; below it this is a no-op so short
/// sessions stay full-fidelity and byte-stable (append-only). Above it,
/// permanently stubs old (non-active-turn, non-`read_file`) ToolResults in
/// `conv.messages`. Idempotent + monotonic via `compact_old_tool_results_in_place`
/// (`keep_recent_turns = 1` → keeps everything after the last `Role::User`,
/// i.e. the active turn). Because it commits, the stubbed prefix never
/// changes again across turns — the property `microcompact` violated.
pub(crate) fn collapse_committed(conv: &mut crate::conversation::Conversation, token_budget: usize) {
    let threshold_chars = (token_budget as u64 * 4 * 70 / 100) as usize;
    let total_chars: usize = conv
        .messages
        .iter()
        .map(|m| match &m.content {
            MessageContent::ToolResult(r) => r.output.len(),
            MessageContent::Text(t) => t.len(),
            _ => 100,
        })
        .sum();
    if total_chars < threshold_chars {
        return;
    }
    compact_old_tool_results_in_place(conv, /* keep_recent_turns */ 1, /* exempt_read_file */ true);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p atomcode-core collapse_committed 2>&1 | tail -20`
Expected: all three `collapse_committed_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/ctx/render.rs
git commit -m "feat(ctx): add committed, idempotent collapse_committed (replaces microcompact)"
```

---

## Task 3: Core regression — prefix stays byte-frozen across turns

This is the central acceptance test: the property whose absence let the cache break. Once a tool result is stubbed, it must stay byte-identical on every later turn.

**Files:**
- Test: `crates/atomcode-core/src/ctx/render.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn collapse_committed_freezes_stubbed_prefix_across_turns() {
        use crate::tool::{ToolCall, ToolResult};
        // Low budget (threshold = 1_000*4*70/100 = 2_800 chars) on purpose:
        // after the first collapse shrinks the conv, the SECOND collapse must
        // still be above threshold so it actually re-fires and re-examines the
        // already-stubbed prefix — otherwise the freeze assertion is vacuous.
        let budget = 1_000;

        let add_turn = |conv: &mut Conversation, n: usize| {
            conv.add_user_message(&format!("task {n}"));
            let id = format!("c{n}");
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall { id: id.clone(), name: "bash".into(), arguments: "{}".into() }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: id,
                output: format!("[elapsed: 0.0s, exit: 0]\n{}", "x".repeat(6_000)),
                success: true,
            });
        };

        let mut conv = Conversation::new();
        for n in 0..5 {
            add_turn(&mut conv, n);
        }
        collapse_committed(&mut conv, budget);

        // Snapshot every ALREADY-stubbed tool result (output starts with '[').
        let stubbed_before: Vec<(usize, String)> = conv
            .messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| match &m.content {
                MessageContent::ToolResult(r) if r.output.starts_with('[')
                    && r.output.contains("lines, first:") =>
                {
                    Some((i, r.output.clone()))
                }
                _ => None,
            })
            .collect();
        assert!(!stubbed_before.is_empty(), "expected stubs after first collapse");

        // A new turn arrives; collapse again (the just-aged turn now stubs).
        add_turn(&mut conv, 5);
        collapse_committed(&mut conv, budget);

        // Every previously-stubbed result must be byte-identical (monotonic, frozen).
        for (i, before) in &stubbed_before {
            match &conv.messages[*i].content {
                MessageContent::ToolResult(r) => assert_eq!(
                    &r.output, before,
                    "stub at message #{i} mutated across turns — prefix cache would break"
                ),
                other => panic!("message #{i} changed content variant: {:?}", other),
            }
        }
    }
```

- [ ] **Step 2: Run test to verify it passes immediately**

Run: `cargo test -p atomcode-core collapse_committed_freezes_stubbed_prefix_across_turns 2>&1 | tail -20`
Expected: PASS (the implementation from Task 2 already guarantees this; this test pins the invariant against regressions).

> If it FAILS, do not proceed — the monotonic/idempotent guarantee is broken. Re-check Task 2's `compact_old_tool_results_in_place` idempotence (stub `< MIN_COLLAPSE_SIZE` skip).

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-core/src/ctx/render.rs
git commit -m "test(ctx): pin byte-frozen prefix invariant across turns"
```

---

## Task 4: Wire `collapse_committed` into the send path; stop calling `microcompact` in `build_messages`

**Files:**
- Modify: `crates/atomcode-core/src/turn/runner.rs:124-128`
- Modify: `crates/atomcode-core/src/ctx/render.rs:312-313` (remove the microcompact call + threshold local)

- [ ] **Step 1: Insert the committed collapse before the actual-send render**

In `turn/runner.rs`, the actual-send render is at lines 124–128:

```rust
        let context_window = self.ctx.ctx_window();

        let (messages, ctx_stats) =
            self.ctx
                .build_messages(conversation, system_prompt, turn_reminder);
```

Insert one line between `context_window` and the `build_messages` call (`conversation` is `&mut Conversation` per the `run` signature at runner.rs:62-64):

```rust
        let context_window = self.ctx.ctx_window();

        // Commit-collapse old tool results (idempotent, monotonic) so the
        // sent prefix stays byte-stable across turns and the provider
        // prompt-cache holds. Replaces the removed ephemeral microcompact.
        crate::ctx::render::collapse_committed(conversation, context_window);

        let (messages, ctx_stats) =
            self.ctx
                .build_messages(conversation, system_prompt, turn_reminder);
```

- [ ] **Step 2: Remove the microcompact call from `build_messages`**

In `render.rs`, delete lines 312-313 (the threshold local and the call):

```rust
    let microcompact_threshold = (token_budget as u64 * 4 * 70 / 100) as usize;
    microcompact(&mut result, conv.messages.len(), microcompact_threshold);
```

Leave the surrounding comment block (291-329) — but update the stale claim. Replace the comment paragraph at 291-311 with a one-line pointer:

```rust
    // Prior-turn ToolResult stubbing now happens via the COMMITTED
    // `collapse_committed` (called on `conv.messages` before render, in
    // turn/runner.rs) — NOT here. build_messages stays a pure renderer so
    // the rendered prefix is byte-stable across turns. The 80% FINAL BYTE
    // CEILING below remains the render-time overflow backstop.
```

(The `NOTE (prompt-cache)` block about `replace_stale_reads` at 315-329 stays as-is.)

- [ ] **Step 3: Build — expect microcompact-test breakage only**

Run: `cargo build -p atomcode-core 2>&1 | tail -20`
Expected: library compiles (the `microcompact` fn still exists, just unused → a dead-code warning). Test compilation is handled in Task 5.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-core/src/turn/runner.rs crates/atomcode-core/src/ctx/render.rs
git commit -m "feat(ctx): collapse old tool results committed on the send path; drop microcompact from build_messages"
```

---

## Task 5: Delete `microcompact` and migrate its tests

**Files:**
- Modify: `crates/atomcode-core/src/ctx/render.rs` — delete `microcompact` fn (~925-1001) and reconcile its tests

- [ ] **Step 1: Delete the `microcompact` function**

Remove the entire `fn microcompact(...)` (from the doc-comment at ~902 through the closing `}` at ~1001). The helper `build_call_id_to_tool_map` (used by `compact_old_tool_results_in_place`) and `build_compact_stub` MUST remain.

- [ ] **Step 2: Build the test target to list breakage**

Run: `cargo test -p atomcode-core --no-run 2>&1 | grep -E "cannot find function .microcompact|error" | head`
Expected: errors only in tests that call `microcompact(...)` directly.

- [ ] **Step 3: Reconcile each broken / now-redundant test (deterministic rule)**

Apply this exact rule per failing test:

1. **Tests that call `microcompact(&mut msgs, ...)` directly** — these tested the ephemeral function that no longer exists. Their behavior is now covered by the `collapse_committed_*` tests added in Tasks 2–3. **Delete** them:
   - `microcompact_uses_generic_format_with_tool_label_from_call_id`
   - `microcompact_preserves_current_turn_in_full`
   - `microcompact_is_idempotent_no_double_stub`
   - `microcompact_respects_threshold_parameter`
   - `microcompact_scales_with_window`

2. **`microcompact_skips_read_file_to_preserve_long_session_context`** (calls `build_messages`, asserts read_file stays full) — read_file is now exempted by `collapse_committed`, not by `build_messages`. **Rewrite** its single render call: replace

   ```rust
        let (msgs, _) = build_messages(&conv, "sys", 40_000, "");
   ```

   with

   ```rust
        collapse_committed(&mut conv, 40_000);
        let (msgs, _) = build_messages(&conv, "sys", 40_000, "");
   ```

   (Everything else in that test is unchanged — it still asserts `c_read` is full and at least one bash is `[bash ok: ...]`. Note `conv` must be `let mut conv` — it already is.)

3. **Any other `build_messages` test that asserts a prior-turn ToolResult was stubbed** (search: `cargo test -p atomcode-core --no-run` then run the render tests and inspect failures) — apply the same mechanical fix as (2): insert `collapse_committed(&mut conv, <same budget>);` on the line before the `build_messages(&conv, …, <budget>, …)` call. Below-threshold / current-turn / overflow-ceiling tests need no change.

- [ ] **Step 4: Run the full render test module**

Run: `cargo test -p atomcode-core ctx::render 2>&1 | tail -30`
Expected: all PASS. If a `build_messages`-based test still fails on a missing stub, apply rule (2)/(3) to it; if it fails because it asserted *no* stub and now there is one, the budget was below threshold — leave it and recheck the assertion.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/ctx/render.rs
git commit -m "refactor(ctx): delete microcompact; migrate tests to collapse_committed"
```

---

## Task 6: Full verification

**Files:** none (verification only)

- [ ] **Step 1: Workspace build + lint**

Run: `cargo build --workspace 2>&1 | tail -15`
Expected: success, no errors. Resolve any dead-code warning for now-unused items by deleting them.

Run: `cargo clippy -p atomcode-core 2>&1 | tail -25`
Expected: no new warnings introduced by these files.

- [ ] **Step 2: Run the affected crate's tests**

Run: `cargo test -p atomcode-core 2>&1 | tail -30`
Expected: all PASS, including `collapse_committed_*`, `compact_old_*`, the emergency-compaction tests (`proactive_tier1_*`, `collapse_keeps_last_n_turns_full`, etc.), and the migrated read_file test.

- [ ] **Step 3: Targeted invariant re-run**

Run: `cargo test -p atomcode-core collapse_committed_freezes_stubbed_prefix_across_turns -- --nocapture 2>&1 | tail -10`
Expected: PASS — the byte-frozen-prefix guarantee holds.

- [ ] **Step 4: Final commit (if any cleanup)**

```bash
git add -A
git commit -m "chore(ctx): cleanup after cache-friendly compaction" --allow-empty
```

---

## Done criteria

- `microcompact` is gone; `build_messages` no longer mutates stub state.
- `collapse_committed` runs once before the actual-send render in `turn/runner.rs`, committed + idempotent + monotonic, threshold-gated at `70% × budget × 4` chars, keeps the active turn full, exempts `read_file`.
- Emergency path unchanged (`compact_old_tool_results_in_place(..., 3, false)`).
- 80% `FINAL BYTE CEILING` and emergency Tier-3 untouched → no overflow regression.
- `collapse_committed_freezes_stubbed_prefix_across_turns` passes (the cache invariant).
- `cargo test -p atomcode-core` green.

## Post-merge measurement (not a code task)

After release, re-run the `cache-hit-rca` skill on 4.25.x: `find_pairs --version <new> --day today` → `classify_pairs` → confirm the `tool_*改写/截断` share and the `bad_hit<10%` front-break volume drop. If the one-time first-crossing front break is still material, evaluate spec item ② (fold oldest turns into the frozen `cold_summaries` prefix).
