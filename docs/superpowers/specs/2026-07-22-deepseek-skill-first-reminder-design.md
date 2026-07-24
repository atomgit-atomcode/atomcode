# DeepSeek skill-first reminder (mechanism layer)

**Date:** 2026-07-22
**Status:** Approved, ready for implementation plan
**Scope:** Small — one deepseek-only lifecycle hook. No new subsystem.

## Goal

Make a weak model (deepseek) actually load a matching process skill (e.g.
`brainstorming`) BEFORE it explores the codebase or proposes solutions, on the
opening turn of a task.

## Background / why the persona fix was not enough

A prior fix (`08520767`) added a `SKILL/PROCESS FIRST` bullet to the deepseek-only
`FIRM_EXECUTION_DISCIPLINE` block in `coding_persona`
(`crates/atomcode-coding/src/persona.rs`). Real-machine test (deepseek-v4-flash,
binary built 11:36 containing the fix) showed it **did not hold**: deepseek still
opened with "let me look at the project structure", explored (List/Glob/Read),
wrote a full A/B solution analysis, and only then asked one question via
`request_user_input`. It never called `use_skill(brainstorming)`.

Root cause of the persona approach's failure: a **static** system-prompt line has
weak immediacy for a weak model — it sits near the top of a long prompt, far from
the moment the model decides to act, and competes with the same block's
"FINISH THE JOB / act decisively" MANDATORY bullets. The net signal reads as "go".

GLM-5.2 loads the skill fine and is out of scope — it does not get
`FIRM_EXECUTION_DISCIPLINE` and follows the soft `## SKILLS:` guidance.

## Design

A new **deepseek-only lifecycle hook** that injects a forceful `<system-reminder>`
at the tail of the request on the opening turn, where recency is high — the same
delivery mechanism `StatusReminderHook` and `TodoHook` already use (per-turn
`pre_request` tail injection), which is materially stronger than a static persona
line.

### Component: `SkillFirstHook`

New unit in `atomcode-coding` (e.g. `crates/atomcode-coding/src/skill_first.rs`).

- **State (computed once at construction):** `enabled: bool` =
  `model_needs_firm_execution(model)` AND the skill catalog is non-empty.
  - Gate to deepseek via the existing `model_needs_firm_execution` predicate
    (`persona.rs`) — expose it `pub(crate)` (or add a thin `pub(crate)` wrapper).
    GLM / frontier never get the hook.
  - Skip when no skills are installed (catalog empty) — never nudge toward an
    unmounted `use_skill`/`brainstorming`, mirroring the `TodoHook` /
    `request_user_input` gating discipline. The catalog string is already computed
    at `prepare()` time (passed to `SkillCatalogHook::new(skill_catalog)`), so the
    non-empty check is available at construction.
- **Behavior:** implement `LifecycleHooks::pre_request(&self, messages, ctx)`:
  - Return immediately unless `enabled`.
  - Fire only on the **opening turn**: `ctx.turn_id == 1 && ctx.round == 1`.
    One-shot; adds nothing to later turns or later rounds, so no per-turn noise on
    ongoing coding work.
  - Append one `<system-reminder>` message at the tail (mirror
    `StatusReminderHook`'s exact injection: same wrapping helper / message role).
- **Reminder text (pure function, testable):**
  > Before you explore the codebase, plan, or propose a solution: check the
  > "=== AVAILABLE SKILLS ===" catalog above. If this request matches a skill's
  > description — a design / build / "help me figure out / plan this" request
  > matches `brainstorming` — you MUST call `use_skill` with that skill NOW and let
  > it drive: ask the user ONE question at a time and do NOT pre-decide the solution
  > or start exploring first. If nothing in the catalog matches, proceed normally.

### Wiring

Register in `crates/atomcode-coding/src/parts.rs` `prepare()`, alongside the other
hooks (after `TodoHook`, ~line 539). Construct with the resolved model
(`cfg.model`) and the catalog non-empty flag. Because both TUI/CLI and daemon
`/chat` (webui) run the identical `CodingRuntime` → `prepare()` → `assemble()`
pipeline, this reaches **both** surfaces (unlike the earlier persona-only note,
there is no webui gap here).

## Explicitly out of scope

- **Intent classification by atomcode** (keyword/heuristic matching on the user
  message). Rejected as fragile (bilingual keyword lists rot; re-implements the
  skill-catalog's own description-matching). The reminder lets the model do the
  matching — it just forces the check at the decision point. Gating is by
  turn position + model, not by guessing intent.
- **Force auto-loading the skill** (expanding brainstorming's content client-side
  and injecting it, bypassing the model). Heavier and misfires worse on a false
  positive. Deferred: revisit only if the reminder proves insufficient on real
  hardware.
- **All models / GLM.** deepseek-only.
- **Mid-session design requests** (a design ask arriving at turn 5, not turn 1).
  YAGNI — the dominant case is the opening message. Not handled this round.

## Testing

- Pure reminder-text builder: unit test it returns the expected string.
- Gating (construct-time `enabled`): deepseek + non-empty catalog → enabled;
  GLM → disabled; deepseek + empty catalog → disabled.
- `pre_request` firing: with an enabled hook, `turn_id==1 && round==1` appends
  exactly one reminder message; `turn_id==1 && round==2`, `turn_id==2`, or a
  disabled hook append nothing. (Follow `StatusReminderHook`'s existing test
  style for constructing a `TurnCtx` and asserting on the message tail.)
- Run existing `atomcode-coding` tests — no signature changes to `coding_persona`
  or `prepare` call sites beyond adding the hook.

## Honest limitation

The mechanism guarantees the skill-first directive is put in front of deepseek at
the opening turn with high recency. It does **not** guarantee deepseek then obeys
the skill's "one question at a time / don't pre-solution" discipline once loaded —
that remains partly model behavior. If real-machine testing shows deepseek loads
the skill but still violates its interview discipline, that is a separate problem
(skill-internal discipline), addressed separately. The success criterion for THIS
change is narrower: deepseek calls `use_skill(brainstorming)` on the opening turn
instead of diving straight into exploration.
