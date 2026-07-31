# `request_user_input` UI: optional custom-answer row + Submit spacing

**Date:** 2026-07-22
**Status:** Approved, ready for implementation plan
**Scope:** Medium — two related tweaks to the `request_user_input` panel (tool + TUI + webui + daemon for #1; TUI render only for #2). No kernel change.

## Goal

Two polish fixes to the structured-question panel:

1. **Optional custom-answer row.** The "输入自己的答案…" (Other free-text) row is
   currently appended UNCONDITIONALLY to every single/multiple question. Make it
   controllable per question via a `custom` flag (default `true` = current
   behavior). When the model's options are exhaustive it sets `custom: false` and
   the free-text row disappears. Also guide the model NOT to add its own
   "其他 / Other / catch-all" option (which currently duplicates the auto row).
2. **Submit-row spacing.** In multiple mode, the `✔ 提交` Submit row currently sits
   directly under the last option, visually crammed. Add one blank spacer row
   above it so Submit reads as separate from the choices.

## Background / reference

- Current: `UserInputPanel` always has an Other row at index `options.len()`
  (`crates/atomcode-tuix/src/state.rs`), and `build_user_input_rows`
  (`crates/atomcode-tuix/src/render/retained.rs`) always renders it; the tool
  schema has no way to suppress it.
- opencode's `question` tool (verified from local source) has a per-question
  `custom: Boolean` (default true) — "Allow typing a custom answer" — and its tool
  description says: *"When `custom` is enabled (default), a 'Type your own answer'
  option is added automatically; don't include 'Other' or catch-all options."* This
  design mirrors that.

## Design

### #1 — the `custom` flag

**Tool (`crates/atomcode-capabilities/src/tools/request_user_input.rs`):**
- Add `pub custom: bool` to `UserInputRequest`, `#[serde(default = "…true")]` so an
  absent `custom` deserializes to `true` (backward-compatible: existing callers and
  the current UI behavior are unchanged).
- Add `custom` to the per-question JSON schema (`"type": "boolean"`), and to the
  `questions[]` item schema. Update the tool `description`: a free-text
  "type your own answer" row is added automatically unless you set `custom: false`;
  set `custom: false` when your `options` are exhaustive; do NOT add your own
  "Other"/catch-all option.
- `custom` rides the payload for both the flat single question and each item of the
  batch `questions[]` array (it is a field of `UserInputRequest`, already serialized).

**TUI state (`crates/atomcode-tuix/src/state.rs`):**
- `UserInputPanel` gains `custom: bool` (from the request). When `custom == false`,
  the Other row does not exist:
  - `other_index` / `last_row` / the cursor range / the `checked` vec length /
    `build_response` (no custom-text branch) / `is_other_row` all account for its
    absence. When `custom == true`, every one of these is byte-for-byte the current
    behavior.
- `UserInputBatch` per-question panels inherit each question's own `custom` (they
  are built from `UserInputRequest` via `UserInputPanel::new`).

**TUI render (`crates/atomcode-tuix/src/render/retained.rs`):**
- `build_user_input_rows` renders the Other row only when `custom == true`;
  `user_input_panel_row_count` drops the Other row's rows (and its checkbox slot in
  multiple mode) when `custom == false`. The view (`UserInputPanelView`) carries
  `custom`.

**webui (`webui/src/components/UserInputCard.tsx`, `webui/src/api.ts`):**
- `UserInputQuestion` / `UserInputRequestEvent` gain `custom?: boolean` (absent ⇒
  treated as `true`). The "Other" radio/checkbox + free-text input render only when
  `custom !== false`.

**daemon (`crates/atomcode-daemon/src/live_api.rs`):**
- Forward `custom` on the single-question `user_input_request` event (the batch path
  already carries it inside the `questions` array).

### #2 — Submit-row spacing (multiple mode only)

**TUI render only (`crates/atomcode-tuix/src/render/retained.rs`):**
- In `build_user_input_rows`, for multiple mode, push one blank spacer row before the
  Submit row. Bump `user_input_panel_row_count` by 1 for multiple mode so the
  row-count invariant (`row_count == build_user_input_rows(..).len()`) holds.
- Single mode has no Submit row → unaffected.

## Out of scope (deferred)

- Changing multiple-mode selection semantics.
- Enforcing that the model actually omits its own "Other" option — a duplicate is
  cosmetic, not fatal; the tool-description guidance mitigates it.
- A `custom` control for `text` mode (text mode has no options / Other row; N/A).

## Testing

- **Tool:** `custom` defaults to `true` when absent; parses `false` when present;
  the schema includes `custom`. Existing parse/format tests unchanged.
- **TUI state:** a `custom == false` single panel has no Other row (cursor range
  ends at the last concrete option; `build_response` has no custom-text branch);
  a `custom == false` multiple panel's Submit index shifts down by one; `custom ==
  true` panels are unchanged.
- **TUI render:** `custom == false` drops the Other row(s) and the row-count matches
  `build_user_input_rows(..).len()`; multiple mode gains exactly one blank row above
  Submit and the count matches; single/multiple `custom == true` panels keep their
  current row counts except the new multiple-mode +1 blank.
- **webui:** the Other row is hidden when `custom === false`, shown otherwise.
- Run existing `request_user_input` + tuix panel tests — single-question default
  (`custom` absent ⇒ true) must be unchanged except the multiple-mode Submit blank.

## Risks / notes

- **Row-count invariant** (`user_input_panel_row_count == build_user_input_rows.len()`)
  is the main hazard — both the `custom == false` path and the new multiple-mode
  blank must be reflected in BOTH functions. The existing invariant tests + new
  cases guard it.
- **Existing tests** assert specific multiple-mode row counts; the +1 blank will
  change those expected numbers — update them as part of the change.
- **Working-tree note:** `crates/atomcode-tuix/src/state.rs` currently has unrelated
  uncommitted changes (not part of this work). Implementation must stage only this
  change's hunks (`git add -p`) and never commit the unrelated WIP.
