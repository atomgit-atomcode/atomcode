# Batch User Questions Persona Nudge — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a persona rule telling the model to put multiple user questions into ONE `request_user_input` call's `questions[]` array (the existing Tab-navigated batch form) instead of emitting N separate single-question calls.

**Architecture:** One clause added to the already-gated `REQUEST_USER_INPUT_USAGE` block in the coding persona. No code/mechanism change — reuses the shipped batch UI. Rides the existing `request_user_input_enabled` gate so it vanishes when the tool is disabled.

**Tech Stack:** Rust, `atomcode-coding` crate, `cargo test`.

## Global Constraints

- The clause lives INSIDE `REQUEST_USER_INPUT_USAGE` (`crates/atomcode-coding/src/persona.rs:336`), so it only appears when `request_user_input_enabled == true` (never nudge toward an unmounted tool).
- Model-agnostic (no per-model gating this round).
- Must reconcile with the existing "One focused question at a time" wording — each question stays focused, but multiple focused questions go in ONE call.
- Neutral wording — no opencode/codex names in code/commits.
- Work on branch `release/v5.0.1`. Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

## File Structure

- `crates/atomcode-coding/src/persona.rs` — the ONLY file changed. Modify the `REQUEST_USER_INPUT_USAGE` string constant (~line 336) and add one gated-behavior unit test in `mod tests`.

---

### Task 1: Add the batching rule to `## ASKING THE USER`

**Files:**
- Modify: `crates/atomcode-coding/src/persona.rs` — `REQUEST_USER_INPUT_USAGE` const (~line 336) and `mod tests`.

**Interfaces:**
- Consumes: existing `coding_persona(model: &str, todo_enabled: bool, request_user_input_enabled: bool) -> String`. Unchanged signature.
- Produces: behavioral — when `request_user_input_enabled == true`, the persona additionally contains the substring `answers them together in one form`; absent when `false`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `crates/atomcode-coding/src/persona.rs` (near the other `request_user_input` persona tests):

```rust
    #[test]
    fn batch_questions_rule_present_only_when_enabled() {
        let on = coding_persona("deepseek-v4-flash", false, true);
        assert!(
            on.contains("answers them together in one form"),
            "enabled → batching rule present"
        );
        assert!(
            on.contains("`questions` array"),
            "enabled → names the questions array"
        );
        let off = coding_persona("deepseek-v4-flash", false, false);
        assert!(
            !off.contains("answers them together in one form"),
            "disabled → batching rule gone with the whole block"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p atomcode-coding --lib batch_questions_rule_present_only_when_enabled`
Expected: FAIL — `on.contains("answers them together in one form")` panics (the clause isn't in the const yet).

- [ ] **Step 3: Insert the batching clause**

In `crates/atomcode-coding/src/persona.rs`, the `REQUEST_USER_INPUT_USAGE` const contains the sentence `One focused question at a time.` followed by `Never ask the user to type a secret`. Insert the batching clause between them. Replace:

```rust
code, the task, or a quick check already answers. One focused question at a time. Never ask the \
user to type a secret (password, API key, token) into the prompt — those come from the \
```

with:

```rust
code, the task, or a quick check already answers. Keep each question focused. If you have MORE \
THAN ONE question for the user at this point, put them ALL into ONE `request_user_input` call's \
`questions` array — do NOT make several `request_user_input` calls in the same turn, and never \
write a multiple-choice question as prose; the user answers them together in one form. Never ask \
the user to type a secret (password, API key, token) into the prompt — those come from the \
```

(This drops the standalone "One focused question at a time." and folds "Keep each question focused" into the batching rule so the two no longer read as "make separate calls".)

- [ ] **Step 4: Run the test to verify it passes + no persona regression**

Run: `cargo test -p atomcode-coding --lib persona`
Expected: PASS — the new `batch_questions_rule_present_only_when_enabled` plus all existing persona tests (the `## ASKING THE USER` block still contains `## ASKING THE USER`, `request_user_input`, `structured interview`, etc.).

- [ ] **Step 5: Verify the string is compiled into the binary (optional sanity)**

Run: `cargo build --bin atomcode && strings target/debug/atomcode | grep -c "answers them together in one form"`
Expected: prints `1`.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-coding/src/persona.rs
git commit -m "feat(persona): batch multiple user questions into one request_user_input call

A weak model (deepseek) emitted 3 separate request_user_input calls instead of one
questions[] batch, so each was a standalone single-question panel with no
back-navigation. Add a rule to the gated ASKING THE USER block: put multiple
questions into ONE request_user_input call's questions array, never several calls
in a turn, never a multiple-choice question as prose. Reuses the shipped batch UI;
mirrors how comparable agents guide the model (tool takes an array + prompt rule,
no runtime coalescing).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- "Add batching rule inside the gated `REQUEST_USER_INPUT_USAGE`" → Task 1 Step 3. ✓
- "Reconcile with 'One focused question at a time'" → Step 3 folds it into "Keep each question focused". ✓
- "Model-agnostic, no mechanism change" → only the const string + a test. ✓
- "Persona test present-when-enabled / absent-when-disabled" → Step 1. ✓
- "Run existing persona tests" → Step 4. ✓
- Runtime coalescing deferred → not implemented, matches spec. ✓

**Placeholder scan:** No TBD/TODO. Every step shows the exact string. ✓

**Type consistency:** No signatures change. The test asserts on `answers them together in one form` and `` `questions` array ``, both appearing verbatim in the Step 3 inserted text. ✓

---

## Execution Notes

- Only `atomcode-coding/persona.rs` is touched; no `core` change, no staleness dance.
- Ships **未真机** for the behavioral effect — verify by asking deepseek/GLM something that surfaces several choices and confirming ONE `request_user_input` with `questions[]` (the Tab form) rather than N calls. If deepseek still won't batch, escalate to the deferred runtime-coalescing fallback.
