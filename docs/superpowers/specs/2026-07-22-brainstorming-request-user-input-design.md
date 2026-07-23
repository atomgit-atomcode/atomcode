# Brainstorming questions → `request_user_input` (persona bridge)

**Date:** 2026-07-22
**Status:** Approved, ready for implementation plan
**Scope:** Small — one persona wording delta. No structural code.

## Goal

Let the questions asked during a brainstorming/design-refinement flow be answered
directly in the UI (TUI panel / webui modal) via the existing `request_user_input`
tool, instead of being written as prose the user has to type free-form replies to.

User constraints, captured during brainstorming:

- **Keep it simple; brainstorming only.** Not "route every clarifying question
  through the UI" — just the brainstorming case, for now.
- **Persona nudge as the mechanism** (not editing the external superpowers skill
  file, not a new skill-detection/injection hook).

## Current state (already exists — nothing to build here)

The end-to-end machinery is already in place and wired:

- **Tool:** `request_user_input` — `crates/atomcode-capabilities/src/tools/request_user_input.rs`.
  Modes `single` / `multiple` / `text`; fields `header`, `question`, `mode`, `options`.
- **UI:** TUI footer panel (`atomcode-tuix` `UserInputPanel`, `render/retained.rs`)
  and webui modal (`webui/src/components/UserInputCard.tsx`). Both already render
  the request and post the answer back.
- **Roundtrip:** kernel `AgentEvent::Request` → driver → `AgentCommand::Respond`
  (`crates/atomcode-kernel/src/request.rs`). Declined/timeout degrade to a
  non-error "no answer" result.
- **Env gate:** `ATOMCODE_REQUEST_USER_INPUT` (default ON). Helper
  `request_user_input_enabled_from_env` in `crates/atomcode-config/src/config/mod.rs:592`;
  intentional duplicate in `crates/atomcode-capabilities/src/tools/mod.rs:191-214`.
- **Persona wiring:** `coding_persona(model, todo_enabled, request_user_input_enabled)`
  — `crates/atomcode-coding/src/persona.rs:78`. All 3 call sites already pass
  `request_user_input_switch_enabled()`:
  - `crates/atomcode-coding/src/assemble.rs:71`
  - `crates/atomcode-coding/src/parts.rs:821`
  - `crates/atomcode-coding/src/parts.rs:994`
- **Existing persona blocks:**
  - `## SKILLS:` (`SKILLS_USAGE`, persona.rs:288) — tells the model to load a
    matching skill (e.g. brainstorming) FIRST and "let it drive the questions."
  - `## ASKING THE USER:` (`REQUEST_USER_INPUT_USAGE`, persona.rs:303) — gated on
    `request_user_input_enabled`; nudges the tool for decisions "genuinely the
    USER'S to make."

## The actual gap

During brainstorming the model reads two blocks that don't connect:

- `## SKILLS:` — brainstorming drives the questions.
- `## ASKING THE USER:` — `request_user_input` is a **scarce gate**: "genuinely the
  USER'S to make", "Ask ONLY for what you genuinely cannot decide, look up, or
  verify yourself", "never for something a quick check already answers."

Brainstorming's job is the opposite of scarce: it deliberately asks many
exploratory clarifying questions (purpose, constraints, approach preferences),
"one at a time, multiple choice preferred." The model does not connect those
questions to `request_user_input` — the "ask sparingly" framing reads as a reason
*not* to — so it writes them as prose instead. The two blocks need a bridge.

## Change

A single wording delta in `REQUEST_USER_INPUT_USAGE` (`crates/atomcode-coding/src/persona.rs:303`):
append a bridging clause, in spirit:

> When a skill (e.g. brainstorming) is driving a round of clarifying / interview-style
> Q&A to refine a design, surface **its** questions through this tool too — use
> `single`/`multiple` with concrete `options` for choice questions, `text` for open
> ones — so the user answers in the UI instead of reading a prose question. The
> "ask sparingly / only what you can't decide yourself" guidance above governs your
> own ad-hoc questions; it does **not** constrain a skill's structured interview.

Properties:

- Lives **inside** `REQUEST_USER_INPUT_USAGE`, so it is automatically governed by
  the existing `request_user_input_enabled` gate — when the tool is disabled the
  clause disappears (never nudge toward an unmounted tool; mirrors the TodoHook
  gating principle).
- No new params, no new call sites, no changes to the external superpowers skill.
- Applies to all models (keep it simple), riding the existing conditional block.

Optionally, a one-clause pointer may be added to `SKILLS_USAGE` (persona.rs:288)
so the two blocks cross-reference each other. This is a nice-to-have, not required;
the plan should treat it as optional and low-risk.

## Explicitly out of scope (deferred)

- **webui `/chat` path.** The daemon `/chat` streaming endpoint builds its system
  prompt via `build_api_system_prompt` (`crates/atomcode-daemon/src/lib.rs:3378`),
  which uses `atomcode_config::config::prompt_sections` / `UNIFIED_PROMPT` and does
  **not** call `coding_persona`. So this nudge will not reach brainstorming done in
  the webui. Same nature as the previously-deferred todo-nudge daemon gap. Note it,
  do not fix it this round.
- Editing the local/vendored superpowers `brainstorming` skill markdown.
- Routing non-brainstorming ad-hoc clarifying questions through the UI (the general
  "all clarifying questions" scope was considered and rejected in favor of "keep it
  simple, brainstorming only").

## Testing

- Persona unit test: assert the bridging clause is present in `coding_persona(...)`
  output when `request_user_input_enabled == true`, and absent when `false`.
- Run existing `atomcode-coding` persona tests (`persona` / `parts`) — no signature
  or call-site changes, so they should stay green.
- Real validation is manual only (start a TUI brainstorming session, confirm the
  request panel appears for choice questions). Per project convention this ships
  "未真机" — user verifies on a real terminal.

## Risks / notes

- Prompt-only change: risk is behavioral (model over- or under-firing the tool),
  not structural. Mitigated by scoping the clause to "a skill is driving the Q&A"
  rather than loosening the general scarcity rule.
- If the added clause makes the model call `request_user_input` for its *own*
  ad-hoc questions too eagerly, tighten the "does not constrain a skill's
  structured interview / does not loosen your own ad-hoc questions" wording.
