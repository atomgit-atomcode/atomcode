# DeepSeek Skill-First Reminder — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make DeepSeek load a matching process skill (e.g. `brainstorming`) before it explores or solutions, by injecting a forceful skill-first `<system-reminder>` on the opening turn.

**Architecture:** A new deepseek-only `LifecycleHooks` implementation (`SkillFirstHook`) that fires once, on `turn_id==1 && round==1`, appending a `<system-reminder>` at the request tail — the same ephemeral per-turn injection mechanism `StatusReminderHook`/`TodoHook` use, which has far higher recency for a weak model than a static persona line. Gated to DeepSeek + a non-empty skill catalog. Registered in `prepare()`, so it reaches both TUI/CLI and daemon/webui (identical `CodingRuntime` pipeline).

**Tech Stack:** Rust, `atomcode-coding` crate, `atomcode-kernel` hook trait, `atomcode-capabilities::reminder`, `cargo test`.

## Global Constraints

- **DeepSeek-only.** Gate via the existing `crate::persona::model_needs_firm_execution(model)` predicate. GLM / frontier never get the hook.
- **Never nudge an unmounted tool.** Also gate on a non-empty skill catalog — when no skills are installed, the hook is a no-op (mirrors `TodoHook` / `request_user_input` gating discipline).
- **Opening turn only, one-shot** (`ctx.turn_id == 1 && ctx.round == 1`). No injection on later rounds/turns — no per-turn noise on ongoing coding.
- **Fire on round 1 (deliberately, unlike `StatusReminderHook`).** The reminder must preempt the model's very first action. The resulting user-after-user tail is safe *because the hook is DeepSeek-only* (OpenAI-compatible API tolerates consecutive user messages; the Anthropic-strict rejection that makes `StatusReminderHook` skip round 1 never applies here).
- **Ephemeral injection**, wrapped in `<system-reminder>` via `atomcode_capabilities::reminder::system_reminder`, appended as `Message::user(...)` — same convention as `TodoHook`/`StatusReminderHook`. Never mutate the persisted user message.
- Reaches both TUI/CLI and daemon/webui (same `CodingRuntime` → `prepare()` pipeline).
- Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Work on the current branch `release/v5.0.1`.

---

## File Structure

- `crates/atomcode-coding/src/skill_first.rs` — **new.** The `SkillFirstHook` unit: construct-time gating, the pure reminder body, and the `pre_request` firing logic. Self-contained + unit-tested.
- `crates/atomcode-coding/src/lib.rs` — add `mod skill_first;` (mirror `mod todo;` at line ~56).
- `crates/atomcode-coding/src/persona.rs:181` — widen `fn model_needs_firm_execution` to `pub(crate) fn` so the hook can reuse the DeepSeek predicate.
- `crates/atomcode-coding/src/parts.rs` — capture a `has_skills` flag before `skill_catalog` is moved (line ~509) and register the hook after `TodoHook` (line ~541).

---

### Task 1: `SkillFirstHook` — the hook unit

**Files:**
- Modify: `crates/atomcode-coding/src/persona.rs:181` (visibility)
- Modify: `crates/atomcode-coding/src/lib.rs` (module declaration, ~line 56)
- Create: `crates/atomcode-coding/src/skill_first.rs`
- Test: `crates/atomcode-coding/src/skill_first.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::persona::model_needs_firm_execution(&str) -> bool` (made `pub(crate)` here); `atomcode_capabilities::reminder::system_reminder(&str) -> String`; `atomcode_kernel::hook::{LifecycleHooks, TurnCtx}`; `atomcode_kernel::message::Message`.
- Produces: `pub struct SkillFirstHook` with `pub fn new(model: &str, has_skills: bool) -> Self`, implementing `LifecycleHooks`. Task 2 constructs it as `crate::skill_first::SkillFirstHook::new(&cfg.model, has_skills)`.

- [ ] **Step 1: Widen the DeepSeek predicate's visibility**

In `crates/atomcode-coding/src/persona.rs`, line 181, change:

```rust
fn model_needs_firm_execution(model: &str) -> bool {
```

to:

```rust
pub(crate) fn model_needs_firm_execution(model: &str) -> bool {
```

- [ ] **Step 2: Declare the module**

In `crates/atomcode-coding/src/lib.rs`, next to `mod todo;` (line ~56), add:

```rust
mod skill_first;
```

- [ ] **Step 3: Create the hook file with a NO-OP `pre_request` and the full tests (red step)**

Create `crates/atomcode-coding/src/skill_first.rs` with the struct, a real `body()`, an intentionally-empty `pre_request` (so the firing tests fail first), and the tests:

```rust
//! `SkillFirstHook` — a DeepSeek-only opening-turn `<system-reminder>` that forces a
//! skill-first check before the model explores or proposes a solution.
//!
//! A weak model (DeepSeek) under-weights the soft `## SKILLS:` guidance and the static
//! `SKILL/PROCESS FIRST` persona line (both proved insufficient on real hardware): it
//! opens by exploring the codebase and pre-solutioning instead of loading a matching
//! process skill like `brainstorming`. This injects the skill-first directive with high
//! recency — at the request TAIL, on the opening turn — the same ephemeral mechanism
//! `TodoHook`/`StatusReminderHook` use.
//!
//! Gated to DeepSeek (via `model_needs_firm_execution`) AND a non-empty skill catalog
//! (never nudge `use_skill` when no skills are installed). One-shot: opening turn only.
//!
//! Unlike `StatusReminderHook` we DO fire on round 1 — the reminder must preempt the
//! model's very first action. The resulting user-after-user tail is safe here because the
//! hook is DeepSeek-only (OpenAI-compatible; consecutive user messages are accepted,
//! unlike the Anthropic-strict rejection that makes `StatusReminderHook` skip round 1).

use async_trait::async_trait;
use atomcode_capabilities::reminder::system_reminder;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;

/// Injects a one-shot skill-first `<system-reminder>` on the opening turn, for DeepSeek only.
pub struct SkillFirstHook {
    /// Precomputed at construction: DeepSeek AND at least one skill installed.
    enabled: bool,
}

impl SkillFirstHook {
    /// Enabled only for a weak model needing firm steering (DeepSeek) AND when the skill
    /// catalog is non-empty (`has_skills`). Anything else yields a no-op hook.
    pub fn new(model: &str, has_skills: bool) -> Self {
        Self {
            enabled: has_skills && crate::persona::model_needs_firm_execution(model),
        }
    }

    /// The forceful skill-first reminder body (pure, testable). Wrapped by
    /// `system_reminder` before injection.
    fn body() -> &'static str {
        "Before you explore the codebase, plan, or propose a solution: check the \
\"=== AVAILABLE SKILLS ===\" catalog above. If this request matches a skill's description \
— a design / build / \"help me figure out / plan this\" request matches `brainstorming` — \
you MUST call `use_skill` with that skill NOW and let it drive: ask the user ONE question \
at a time and do NOT pre-decide the solution or start exploring first. If nothing in the \
catalog matches, proceed normally."
    }
}

#[async_trait]
impl LifecycleHooks for SkillFirstHook {
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        // (implemented in Step 5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(turn_id: u64, round: u32) -> TurnCtx {
        TurnCtx {
            turn_id,
            round,
            ..Default::default()
        }
    }

    #[test]
    fn body_names_use_skill_brainstorming_and_one_at_a_time() {
        let b = SkillFirstHook::body();
        assert!(b.contains("use_skill"), "{b}");
        assert!(b.contains("brainstorming"), "{b}");
        assert!(b.contains("ONE question at a time"), "{b}");
    }

    #[tokio::test]
    async fn deepseek_opening_turn_injects_one_wrapped_reminder() {
        let hook = SkillFirstHook::new("deepseek-v4-flash", true);
        let mut msgs = vec![Message::system("s"), Message::user("hi")];
        hook.pre_request(&mut msgs, &ctx(1, 1)).await;
        assert_eq!(msgs.len(), 3, "opening turn appends exactly one reminder");
        assert!(
            msgs[2].text.starts_with("<system-reminder>") && msgs[2].text.contains("use_skill"),
            "wrapped skill-first reminder: {:?}",
            msgs[2].text
        );
    }

    #[tokio::test]
    async fn does_not_fire_after_the_opening_turn() {
        let hook = SkillFirstHook::new("deepseek-v4-flash", true);
        // Round 2 of turn 1 — too late, and would double-inject.
        let mut a = vec![Message::user("hi"), Message::assistant("a", vec![])];
        let before_a = a.clone();
        hook.pre_request(&mut a, &ctx(1, 2)).await;
        assert_eq!(a, before_a, "must not fire on later rounds");
        // Turn 2 — a fresh user message later in the session.
        let mut b = vec![Message::user("hi")];
        let before_b = b.clone();
        hook.pre_request(&mut b, &ctx(2, 1)).await;
        assert_eq!(b, before_b, "must not fire on later turns");
    }

    #[tokio::test]
    async fn disabled_for_glm_frontier_and_empty_catalog() {
        for (model, has_skills) in [
            ("glm-5.2", true),
            ("m", true),
            ("deepseek-v4-flash", false),
        ] {
            let hook = SkillFirstHook::new(model, has_skills);
            let mut msgs = vec![Message::user("hi")];
            let before = msgs.clone();
            hook.pre_request(&mut msgs, &ctx(1, 1)).await;
            assert_eq!(
                msgs, before,
                "must be a no-op for (model={model}, has_skills={has_skills})"
            );
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify the firing tests FAIL**

Run: `cargo test -p atomcode-coding --lib skill_first`
Expected: `body_names_...` and `disabled_...` PASS (no-op hook injects nothing, which matches the disabled expectation), but `deepseek_opening_turn_injects_one_wrapped_reminder` and `does_not_fire_after_the_opening_turn` — specifically the *opening-turn* one — FAIL (asserts `msgs.len() == 3` but the no-op left it at 2).

- [ ] **Step 5: Implement `pre_request`**

Replace the no-op `pre_request` body with:

```rust
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        if !self.enabled {
            return;
        }
        // Opening turn only (one-shot). We DO fire on round 1 (see module doc): the
        // reminder must land before the model's first action, and the user-after-user
        // tail is safe because this hook is DeepSeek-only (OpenAI-compatible).
        if ctx.turn_id != 1 || ctx.round != 1 {
            return;
        }
        messages.push(Message::user(system_reminder(Self::body())));
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p atomcode-coding --lib skill_first`
Expected: all four tests PASS.

- [ ] **Step 7: Confirm no regressions in the crate**

Run: `cargo test -p atomcode-coding`
Expected: full suite PASS (including the existing `model_needs_firm_execution_is_deepseek_only` at persona.rs — visibility change does not affect behavior).

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-coding/src/skill_first.rs crates/atomcode-coding/src/lib.rs crates/atomcode-coding/src/persona.rs
git commit -m "feat(coding): SkillFirstHook — deepseek opening-turn skill-first reminder

A weak model (deepseek) skips use_skill and dives into exploring/solutioning;
the static SKILL/PROCESS FIRST persona line did not hold on real hardware. This
adds a deepseek-only lifecycle hook that injects a forceful skill-first
<system-reminder> at the request tail on the opening turn (high recency, same
mechanism as StatusReminderHook/TodoHook). Gated to deepseek + a non-empty skill
catalog. Fires on round 1 by design (must preempt the first action; safe because
deepseek is OpenAI-compatible). Not yet wired into prepare() (next commit).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Wire `SkillFirstHook` into `prepare()`

**Files:**
- Modify: `crates/atomcode-coding/src/parts.rs` (capture `has_skills` before line ~509; register hook after line ~541)

**Interfaces:**
- Consumes: `crate::skill_first::SkillFirstHook::new(&cfg.model, has_skills)` from Task 1; the existing `skill_catalog: Option<String>` local and `cfg.model` in `prepare()`.
- Produces: nothing new — appends one hook to the existing `hooks` vec.

- [ ] **Step 1: Capture a `has_skills` flag before `skill_catalog` is moved**

In `crates/atomcode-coding/src/parts.rs`, the catalog is moved into `SkillCatalogHook::new(skill_catalog)` at line ~509. Immediately BEFORE that line, add the capture. Change:

```rust
    // Skill catalog — leading system message (persona → context → memory → skills), so
    // the model sees which skills are installed and can trigger one on a description
    // match. `None` (no skills) makes the hook a no-op. Reconciles in place on resume.
    hooks.push(Arc::new(SkillCatalogHook::new(skill_catalog)));
```

to:

```rust
    // Skill catalog — leading system message (persona → context → memory → skills), so
    // the model sees which skills are installed and can trigger one on a description
    // match. `None` (no skills) makes the hook a no-op. Reconciles in place on resume.
    // Capture whether any skill is installed BEFORE the catalog is moved — SkillFirstHook
    // (registered below) uses it to stay a no-op when there's nothing to trigger.
    let has_skills = skill_catalog.as_ref().is_some_and(|c| !c.trim().is_empty());
    hooks.push(Arc::new(SkillCatalogHook::new(skill_catalog)));
```

- [ ] **Step 2: Register the hook after `TodoHook`**

In `crates/atomcode-coding/src/parts.rs`, after the `TodoHook` block (line ~539-541):

```rust
    if crate::persona::todo_switch_enabled() {
        hooks.push(Arc::new(crate::todo::TodoHook));
    }
```

add:

```rust
    // DeepSeek-only opening-turn skill-first reminder. A weak model (deepseek) skips
    // use_skill and dives straight into exploring/solutioning; a static persona line did
    // not hold. This injects a forceful <system-reminder> on the opening turn only, where
    // recency is high. Gated to deepseek (model_needs_firm_execution) + a non-empty skill
    // catalog (never nudge use_skill when no skills are installed). No-op otherwise.
    hooks.push(Arc::new(crate::skill_first::SkillFirstHook::new(
        &cfg.model,
        has_skills,
    )));
```

- [ ] **Step 3: Build and run the crate suite**

Run: `cargo test -p atomcode-coding`
Expected: compiles and the full suite PASSES (no behavior change to existing hooks; the new hook is appended).

- [ ] **Step 4: Verify the reminder is compiled into the atomcode binary**

Run: `cargo build --bin atomcode && strings target/debug/atomcode | grep -c "ONE question at a time"`
Expected: prints `1` (the reminder body is baked into the binary).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-coding/src/parts.rs
git commit -m "feat(coding): register SkillFirstHook in prepare()

Wire the deepseek-only opening-turn skill-first reminder into the canonical hook
chain (after TodoHook), gated on deepseek + a non-empty skill catalog. Reaches
both TUI/CLI and daemon/webui via the shared CodingRuntime pipeline.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- "Deepseek-only lifecycle hook" → Task 1 (`SkillFirstHook`, gated via `model_needs_firm_execution`). ✓
- "Non-empty catalog gate" → Task 1 `new(model, has_skills)` + Task 2 `has_skills` capture. ✓
- "Opening turn only, one-shot; fire on round 1" → Task 1 Step 5 (`turn_id==1 && round==1`) + module doc rationale. ✓
- "Tail `<system-reminder>` via `system_reminder`, `Message::user`" → Task 1 Step 5. ✓
- "Register in `prepare()`, reaches TUI + webui" → Task 2. ✓
- "Pure reminder-text builder tested; gating tested; firing tested" → Task 1 Steps 3/6. ✓
- "Run existing tests" → Task 1 Step 7, Task 2 Step 3. ✓
- Rejected/deferred (intent classification, force-load, all-models, mid-session) → not implemented, matches spec out-of-scope. ✓

**Placeholder scan:** No TBD/TODO. Every code step shows exact content. The Step 3 no-op `pre_request` is an intentional red-step stub, replaced verbatim in Step 5. ✓

**Type consistency:** `SkillFirstHook::new(model: &str, has_skills: bool)` is defined in Task 1 and called identically in Task 2. `body()` returns `&'static str`, wrapped by `system_reminder(&str) -> String`, pushed as `Message::user(String)`. `TurnCtx { turn_id: u64, round: u32, ..Default::default() }` matches the kernel definition. ✓

---

## Execution Notes

- Only `atomcode-coding` is touched; no `core` change, so no `touch core/lib.rs` staleness dance. `#[tokio::test]` and `async-trait` are already available in the crate.
- After merge this ships **未真机** for the behavioral effect — whether deepseek now calls `use_skill(brainstorming)` on the opening turn is only observable by the user on a real terminal (rebuild `target/debug/atomcode`, run deepseek-v4-flash, send the design request). Per the spec's honest-limitation note, the hook guarantees delivery, not the model's subsequent adherence to the skill's one-at-a-time discipline.
