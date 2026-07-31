# Brainstorming → request_user_input Persona Bridge — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the model route a skill-driven brainstorming/interview's choice questions through the existing `request_user_input` tool (answerable in the TUI/webui UI) instead of writing them as prose.

**Architecture:** Prompt-only change. All machinery (tool, TUI panel, webui modal, kernel roundtrip, env gate, `coding_persona` param + call sites) already exists and is wired. The single gap is that the persona's `## SKILLS:` and `## ASKING THE USER:` blocks don't connect during brainstorming — the "ask sparingly" framing reads as a reason NOT to use the tool for exploratory questions. We add one bridging clause inside the already-gated `REQUEST_USER_INPUT_USAGE` block.

**Tech Stack:** Rust, `atomcode-coding` crate, `cargo test`.

## Global Constraints

- Change lives INSIDE `REQUEST_USER_INPUT_USAGE` (`crates/atomcode-coding/src/persona.rs:303`) so it is automatically governed by the existing `request_user_input_enabled` gate — when the tool is off (`ATOMCODE_REQUEST_USER_INPUT=0`), the clause must disappear with the rest of the block. Never nudge toward an unmounted tool.
- No new function params, no new call sites, no changes to the external superpowers skill files.
- webui `/chat` path (`build_api_system_prompt`, does not use `coding_persona`) is explicitly out of scope this round.
- Do not weaken the general scarcity rule for the model's OWN ad-hoc questions; scope the new clause to "a skill is driving the Q&A."
- Commit message trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`. Current branch is `release/v5.0.1`; commit there (already the working branch).

---

## File Structure

- `crates/atomcode-coding/src/persona.rs` — the ONLY production file changed. Modify the `REQUEST_USER_INPUT_USAGE` string constant (line ~303). Add one test in the existing `mod tests` (line ~406).

No new files.

---

### Task 1: Add the brainstorming bridge clause to `## ASKING THE USER`

**Files:**
- Modify: `crates/atomcode-coding/src/persona.rs` — `REQUEST_USER_INPUT_USAGE` const (~line 303–311)
- Test: `crates/atomcode-coding/src/persona.rs` — `mod tests` (~line 406)

**Interfaces:**
- Consumes: existing `coding_persona(model: &str, todo_enabled: bool, request_user_input_enabled: bool) -> String`. Unchanged signature.
- Produces: no new symbols. Behavioral: when `request_user_input_enabled == true`, the persona string additionally contains the substring `structured interview`; when `false`, it does not (already guaranteed by the gate).

- [ ] **Step 1: Write the failing test**

Add this test inside `mod tests` in `crates/atomcode-coding/src/persona.rs` (e.g. right after the existing `request_user_input_guidance_gated` test at ~line 425):

```rust
    #[test]
    fn brainstorming_bridge_present_only_when_enabled() {
        let on = coding_persona("deepseek-v4-flash", false, true);
        assert!(
            on.contains("structured interview"),
            "enabled → brainstorming bridge clause present"
        );
        assert!(
            on.contains("brainstorming"),
            "enabled → clause names the brainstorming case"
        );
        let off = coding_persona("deepseek-v4-flash", false, false);
        assert!(
            !off.contains("structured interview"),
            "disabled → bridge clause gone with the whole block"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p atomcode-coding brainstorming_bridge_present_only_when_enabled`
Expected: FAIL — the `on.contains("structured interview")` assertion panics (`enabled → brainstorming bridge clause present`), because the clause isn't in the const yet.

- [ ] **Step 3: Append the bridge clause to the const**

In `crates/atomcode-coding/src/persona.rs`, the `REQUEST_USER_INPUT_USAGE` const currently ends like this:

```rust
for what you genuinely cannot decide, look up, or verify yourself — never for something the \
code, the task, or a quick check already answers. One focused question at a time. Never ask the \
user to type a secret (password, API key, token) into the prompt — those come from the \
environment or a secrets store, not a question.";
```

Change the final line so the string continues instead of closing, and append the clause. Replace:

```rust
environment or a secrets store, not a question.";
```

with:

```rust
environment or a secrets store, not a question. \
When a skill (for example brainstorming) is driving a round of clarifying, interview-style \
questions to refine a design, surface ITS questions through this tool too: use `single` or \
`multiple` with concrete `options` for choice questions and `text` for an open answer, so the \
user answers in the UI instead of reading a prose question. The 'ask sparingly, only for what \
you cannot decide yourself' guidance above governs YOUR OWN ad-hoc questions; it does not \
constrain a skill's structured interview.";
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p atomcode-coding brainstorming_bridge_present_only_when_enabled`
Expected: PASS.

- [ ] **Step 5: Run the full persona test suite (no regressions)**

Run: `cargo test -p atomcode-coding persona`
Expected: all persona tests PASS (including the pre-existing `request_user_input_guidance_gated`, which still holds because the clause is inside the same gated block).

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-coding/src/persona.rs
git commit -m "feat(persona): bridge brainstorming questions to request_user_input

The SKILLS block tells the model to let a skill drive the questions; the
ASKING THE USER block frames request_user_input as a scarce gate. During
brainstorming those two don't connect, so the model writes prose questions
instead of surfacing them in the UI. Add one bridging clause inside the
already-gated REQUEST_USER_INPUT_USAGE block: when a skill is driving a
clarifying/interview flow, route its choice questions through the tool
(single/multiple with options; text for open), while leaving the scarcity
rule for the model's own ad-hoc questions intact.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2 (OPTIONAL): Cross-reference the bridge from `## SKILLS`

Low-value, low-risk polish. The `SKILLS_USAGE` block already says brainstorming should "let it drive the questions"; this adds a pointer so the two blocks reference each other. Skip if you prefer the minimal diff — Task 1 stands alone.

**Files:**
- Modify: `crates/atomcode-coding/src/persona.rs` — `SKILLS_USAGE` const (~line 288–296)

**Interfaces:**
- Consumes: nothing new.
- Produces: no new symbols. Behavioral: adds a substring `answer in the UI` to the always-present `## SKILLS` block.

- [ ] **Step 1: Write the failing test**

Add inside `mod tests`:

```rust
    #[test]
    fn skills_block_points_at_ui_answering() {
        // Always-present block, independent of the request_user_input gate.
        let p = coding_persona("m", true, false);
        assert!(
            p.contains("answer in the UI"),
            "SKILLS block cross-references answering skill questions in the UI"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p atomcode-coding skills_block_points_at_ui_answering`
Expected: FAIL on `p.contains("answer in the UI")`.

- [ ] **Step 3: Append the pointer to `SKILLS_USAGE`**

The `SKILLS_USAGE` const currently ends:

```rust
use the minimal set; if none match, proceed normally.";
```

Replace with:

```rust
use the minimal set; if none match, proceed normally. When the loaded skill runs an interview \
(for example brainstorming asking questions to refine a design), let the user answer in the UI: \
prefer `request_user_input` for its choice questions when that tool is available.";
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p atomcode-coding skills_block_points_at_ui_answering`
Expected: PASS.

- [ ] **Step 5: Run the full persona suite**

Run: `cargo test -p atomcode-coding persona`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-coding/src/persona.rs
git commit -m "feat(persona): cross-reference UI answering from the SKILLS block

Point the SKILLS guidance at request_user_input for skill-driven interviews
so the SKILLS and ASKING THE USER blocks reference each other.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- "Add bridging clause inside `REQUEST_USER_INPUT_USAGE`, governed by existing gate" → Task 1. ✓
- "No new params/call sites/external-skill edits" → honored (only the const string + a test change). ✓
- "Optional pointer in `SKILLS_USAGE`" → Task 2, marked optional. ✓
- "Persona unit test: present when enabled, absent when disabled" → Task 1 Step 1. ✓
- "Run existing persona tests, stay green" → Task 1 Step 5. ✓
- "webui `/chat` deferred" → not implemented, matches spec out-of-scope. ✓
- "Real validation manual / 未真机" → no automated real-terminal step; correct, user verifies. ✓

**Placeholder scan:** No TBD/TODO; every code step shows the exact string. ✓

**Type consistency:** No signatures change. Test asserts on the literal substring `structured interview`, which appears verbatim in the Step 3 appended text. Task 2 asserts on `answer in the UI`, which appears verbatim in its Step 3 text. ✓

---

## Execution Notes

- Both tasks touch only `crates/atomcode-coding/src/persona.rs`. This crate builds and tests without special feature flags (`persona.rs` is in the default build); no `touch core/lib.rs` staleness dance is needed since `core` is untouched.
- After merging, this ships un-real-machine-tested ("未真机") — the behavioral effect (panel appearing during a live brainstorming session) is only observable by the user on a real terminal.
