# Nudge the model to batch user questions into one `request_user_input` call

**Date:** 2026-07-22
**Status:** Approved, ready for implementation plan
**Scope:** Small — one persona wording addition. No new mechanism.

## Goal

Make the model ask several related questions in ONE `request_user_input` call
(`questions[]` array → the Tab-navigated batch form that already exists) instead
of emitting N separate single-question calls that the user must answer one at a
time with no way to go back.

## Background

The multi-question batch capability already ships (tool `questions[]` array, TUI
Tab-navigated form with a Submit stop, webui sequential stepper, one batched
response). But on a real run deepseek-v4-flash emitted **3 separate**
`request_user_input` calls ("Running 3 request_user_input calls") instead of one
batch call — so each was a standalone single-question panel: answer, submit,
next, with no back-navigation. The batch UI was never fed a batch.

The tool's *description* already mentions the `questions` array, but the model
(a weak model that under-weights tool descriptions) ignored it.

## How opencode and codex solve this (reference, verified from local source)

Both support multi-question forms, and both do it the same way: **one tool call
that carries all the questions**, plus **prompt guidance telling the model to use
it** — neither coalesces N separate calls in the runtime.

- **opencode** (`packages/core/src/tool/question.ts`): a `question` tool whose
  input is `questions: Array`; the TUI renders a Tab/left-right form and submits
  all answers together. Permissions are one-at-a-time; there is no call-coalescing.
- **codex** (`codex-rs/.../collaboration-mode-templates/templates/default.md`):
  a `request_user_input` tool plus MCP elicitation (one request, a JSON Schema
  with multiple properties → a multi-field form), and the explicit prompt rule:
  *"Never write a multiple choice question as a textual assistant message."*

Conclusion: the industry answer is a **prompt nudge** (this design), not
runtime coalescing. atomcode's batch UI already matches opencode's `question`
tool; the only missing piece is guiding the model to make ONE call.

## Design

Add a batching rule to the existing `REQUEST_USER_INPUT_USAGE` block
(`## ASKING THE USER:`, `crates/atomcode-coding/src/persona.rs`), which is
already gated on `request_user_input_enabled` (so the guidance only appears when
the tool is actually mounted). The rule, in spirit:

> When you have MORE THAN ONE question for the user at this point, put them ALL
> into a SINGLE `request_user_input` call's `questions` array — do NOT make
> multiple `request_user_input` calls in the same turn, and never write a
> multiple-choice question as prose. The user then answers them together in one
> form.

Properties:
- Lives inside the already-gated block → disappears when the tool is disabled
  (`ATOMCODE_REQUEST_USER_INPUT=0`), never nudging toward an unmounted tool.
- Applies to all models (the rule is model-agnostic; a weak model needs it most,
  a strong model already tends to comply). No per-model gating this round.
- No code/mechanism change — reuses the shipped batch UI end-to-end.
- Neutral wording (no opencode/codex names in code/commits, per project rule).

## Out of scope (deferred)

- **Runtime coalescing of N separate `request_user_input` calls into one batch
  panel** (the kernel pre-scan + synthesize/de-synthesize approach). Neither
  reference does this; it is heavier and riskier (turn-loop surgery, tool_call/
  result-id pairing). Keep as a fallback ONLY if real-machine testing shows
  deepseek still won't batch after the persona rule.
- Per-model (deepseek-only) strengthening — start model-agnostic; revisit if the
  base rule proves insufficient on deepseek.

## Testing

- Persona unit test: the batching guidance substring is present in
  `coding_persona(...)` when `request_user_input_enabled == true` and absent when
  `false` (rides the existing gate).
- Run existing `atomcode-coding` persona tests — no signature/call-site changes.

## Honest limitation

This is a prompt nudge. It raises the model's tendency to batch but does not
*guarantee* a weak model complies (the same class of risk seen with earlier
persona nudges). The success criterion is behavioral and real-machine-only:
deepseek/GLM, asked something that surfaces several choices, emits ONE
`request_user_input` with `questions[]` (→ the Tab form) rather than N calls. If
deepseek still won't, escalate to the deferred runtime-coalescing fallback.
