# `request_user_input` custom-answer flag + Submit spacing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a per-question `custom` flag (default true) that hides the auto "type your own answer" free-text row when false, plus a blank spacer above the multiple-mode Submit row.

**Architecture:** `custom` is a new field on `UserInputRequest` (serde-default true), threaded to `UserInputPanel` and the render view; the Other row's existence is gated on it across the state index math, the renderer, and the event-loop digit handler. The Submit-spacing tweak is render-only. No kernel change.

**Tech Stack:** Rust (`atomcode-capabilities`, `atomcode-tuix`, `atomcode-daemon`), React/TS (`webui`), `cargo test`.

## Global Constraints

- `custom` defaults to `true` (`#[serde(default = ...)]`) — absent ⇒ current behavior, zero regression for existing callers.
- The Other row exists iff `custom == true`. Every index that counted it (`other_index`, `submit_index`, `last_row`, the `checked` vec length, the render rows, the row-count, the digit handler) must gate on `custom`.
- Row-count invariant: `user_input_panel_row_count(view) == build_user_input_rows(view).len()` must hold for BOTH `custom` values AND with the new multiple-mode blank.
- The Submit blank is multiple-mode only (single mode has no Submit row).
- Neutral wording. Work on `release/v5.0.1`. Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **WIP caution:** `crates/atomcode-tuix/src/{state.rs, event_loop/mod.rs}` may carry unrelated uncommitted changes at implementation time. Stage ONLY this change's hunks (`git add -p <file>`); never commit the unrelated WIP.

---

## File Structure

- `crates/atomcode-capabilities/src/tools/request_user_input.rs` — `custom` field, serde default, schema, description. (Task 1)
- `crates/atomcode-tuix/src/state.rs` — `UserInputPanel.custom` + index math. (Task 2)
- `crates/atomcode-tuix/src/render/mod.rs` + `render/retained.rs` — gate Other row, blank-before-Submit, row count, view field. (Task 3)
- `crates/atomcode-tuix/src/event_loop/mod.rs` — digit-key handler must not jump to the Other row when `custom == false`; view construction passes `custom`. (Task 4)
- `crates/atomcode-daemon/src/live_api.rs`, `webui/src/api.ts`, `webui/src/components/UserInputCard.tsx` — forward + honor `custom`. (Task 5)

---

### Task 1: Tool layer — the `custom` field

**Files:** Modify + test `crates/atomcode-capabilities/src/tools/request_user_input.rs`

**Interfaces:**
- Produces: `UserInputRequest` gains `pub custom: bool` (serde default true). Task 2/3/5 read `req.custom`.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
    #[test]
    fn parse_custom_defaults_true_and_reads_false() {
        // Absent → true (backward compatible).
        let r = parse_args(
            r#"{"header":"H","question":"Q?","mode":"single","options":[{"label":"A"}]}"#,
        )
        .unwrap();
        assert!(r.custom, "custom absent → defaults true");
        // Explicit false.
        let r2 = parse_args(
            r#"{"header":"H","question":"Q?","mode":"single","options":[{"label":"A"}],"custom":false}"#,
        )
        .unwrap();
        assert!(!r2.custom, "custom:false parsed");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p atomcode-capabilities --lib parse_custom_defaults_true_and_reads_false`
Expected: FAIL to compile (`UserInputRequest` has no field `custom`).

- [ ] **Step 3: Add the field with a serde default**

In `crates/atomcode-capabilities/src/tools/request_user_input.rs`, add a default helper and the field. After the `UserInputMode` enum (or near the top of the structs), add:

```rust
fn default_true() -> bool {
    true
}
```

Then change the `UserInputRequest` struct:

```rust
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserInputRequest {
    pub header: String,
    pub question: String,
    pub mode: UserInputMode,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    /// Whether the auto "type your own answer" free-text row is offered
    /// (single/multiple). Default true (absent ⇒ true) — backward compatible.
    /// Set false when `options` are exhaustive.
    #[serde(default = "default_true")]
    pub custom: bool,
}
```

Any struct literal of `UserInputRequest` in this file's tests must add `custom: true` — update the existing `roundtrip_serde` test's literal and the `format_batch_*` test literals to include `custom: true`.

- [ ] **Step 4: Update schema + description**

In `parameters_schema`, add `custom` to the shared `question` object's `properties` (so it applies to both the flat form and `questions[]` items):

```rust
                "options": { /* unchanged */ },
                "custom": {"type": "boolean", "description": "Offer a free-text 'type your own answer' row (default true). Set false when your options are exhaustive."}
```

Update `description`: append to the existing text:

```
 A free-text \"type your own answer\" row is added automatically for single/multiple unless you set `custom` to false — so do NOT add your own \"Other\"/catch-all option; set `custom:false` when your options already cover every case.
```

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p atomcode-capabilities --lib request_user_input`
Expected: PASS (new test + existing, with the updated literals).

- [ ] **Step 6: Commit** (stage only this file)

```bash
git add crates/atomcode-capabilities/src/tools/request_user_input.rs
git commit -m "feat(request_user_input): per-question custom flag (default true)

Add UserInputRequest.custom (serde default true) so the auto 'type your own
answer' row can be suppressed; schema + description tell the model to set
custom:false for exhaustive options and not add its own Other option.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: TUI state — gate the Other row on `custom`

**Files:** Modify + test `crates/atomcode-tuix/src/state.rs`

**Interfaces:**
- Consumes: `UserInputRequest.custom` (Task 1).
- Produces: `UserInputPanel` gains `pub custom: bool`. `submit_index`/`last_row`/`is_other_row`/`checked` length account for it. Task 3/4 read `panel.custom`.

- [ ] **Step 1: Write the failing test**

Add near the `UserInputPanel` (in the file's test module, or a new `#[cfg(test)] mod`):

```rust
#[cfg(test)]
mod user_input_custom_tests {
    use super::*;
    use atomcode_capabilities::tools::request_user_input::{
        UserInputMode, UserInputOption, UserInputRequest,
    };

    fn req(mode: UserInputMode, custom: bool) -> UserInputRequest {
        UserInputRequest {
            header: "H".into(),
            question: "Q?".into(),
            mode,
            options: vec![
                UserInputOption { label: "A".into(), description: None },
                UserInputOption { label: "B".into(), description: None },
            ],
            custom,
        }
    }

    #[test]
    fn single_no_custom_row_when_disabled() {
        let p = UserInputPanel::new(1, &req(UserInputMode::Single, false));
        // Only the 2 concrete options are navigable; no Other row.
        p_move_to_bottom(&mut { p.clone() });
        assert!(!UserInputPanel::new(1, &req(UserInputMode::Single, false)).custom);
        let p2 = UserInputPanel::new(1, &req(UserInputMode::Single, false));
        assert_eq!(p2.last_row_for_test(), 1, "single, no custom → last row = last option (idx 1)");
        let p3 = UserInputPanel::new(1, &req(UserInputMode::Single, true));
        assert_eq!(p3.last_row_for_test(), 2, "single, custom → Other row is last (idx 2)");
    }

    #[test]
    fn multiple_submit_index_shifts_without_custom() {
        let with = UserInputPanel::new(1, &req(UserInputMode::Multiple, true));
        assert_eq!(with.submit_index(), Some(3), "custom → other@2, submit@3");
        let without = UserInputPanel::new(1, &req(UserInputMode::Multiple, false));
        assert_eq!(without.submit_index(), Some(2), "no custom → submit right after options@2");
        assert_eq!(without.checked.len(), 2, "no custom → no Other checkbox slot");
        assert_eq!(with.checked.len(), 3, "custom → Other checkbox slot present");
    }
}
```

Note: `last_row` is currently private. To test it, add a `#[cfg(test)] pub fn last_row_for_test(&self) -> usize { self.last_row() }` to `impl UserInputPanel`, OR make `last_row` `pub(crate)`. Use the `pub(crate)` route (simpler): change `fn last_row` → `pub(crate) fn last_row` and drop the `last_row_for_test` shim + the `p_move_to_bottom` line (delete that stray line). The final test asserts `p2.last_row()` / `p3.last_row()` directly.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p atomcode-tuix --lib user_input_custom`
Expected: FAIL to compile (`custom` field missing on `UserInputRequest` construction is fine — Task 1 added it; the failure is `UserInputPanel` has no `custom` field / `last_row` private).

- [ ] **Step 3: Add `custom` to the struct + `new`**

In `crates/atomcode-tuix/src/state.rs`, add the field to `UserInputPanel` (after `custom_text`):

```rust
    /// Whether the always-appended "Other" free-text row is offered. Mirrors
    /// `UserInputRequest.custom`. When false, the Other row does not exist.
    pub custom: bool,
```

In `UserInputPanel::new`, read it and size `checked` accordingly:

```rust
        // One checkbox slot per concrete option PLUS the trailing "Other" row —
        // but only when custom answers are offered.
        let checked = vec![false; options.len() + r.custom as usize];
        Self {
            request_id,
            header: r.header.clone(),
            question: r.question.clone(),
            mode: r.mode.clone(),
            options,
            cursor: 0,
            checked,
            text: String::new(),
            custom_text: String::new(),
            custom: r.custom,
        }
```

- [ ] **Step 4: Gate the index helpers on `custom`**

Change `submit_index`, `last_row`, `is_other_row` in `impl UserInputPanel`:

```rust
    /// Index of the Submit row (multiple mode only). After the concrete options,
    /// plus the "Other" row when `custom` is on.
    pub fn submit_index(&self) -> Option<usize> {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        if matches!(self.mode, UserInputMode::Multiple) {
            Some(self.options.len() + self.custom as usize)
        } else {
            None
        }
    }

    /// Last navigable cursor index.
    pub(crate) fn last_row(&self) -> usize {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        match self.mode {
            UserInputMode::Multiple => self.submit_index().unwrap(),
            // single/text: the "Other" row is last when custom, else the last option.
            _ => {
                if self.custom {
                    self.other_index()
                } else {
                    self.options.len().saturating_sub(1)
                }
            }
        }
    }

    /// Whether `cursor` is on the always-appended "Other" free-text row.
    pub fn is_other_row(&self) -> bool {
        self.custom && self.cursor == self.other_index()
    }
```

`other_index` stays `self.options.len()` (only meaningful when `custom`). `build_response` needs no change: with `custom == false` the cursor never reaches `options.len()` (single) and `custom_text` stays empty (multiple), so its existing branches are naturally correct.

- [ ] **Step 5: Run tests to verify pass**

Run: `cargo test -p atomcode-tuix --lib user_input_custom`
Expected: PASS.

- [ ] **Step 6: Commit** (stage only your hunks)

```bash
git add -p crates/atomcode-tuix/src/state.rs
git commit -m "feat(tuix): gate the Other free-text row on UserInputPanel.custom

When custom is false the Other row does not exist: submit_index, last_row,
is_other_row and the checked-vec length all drop it. custom=true is unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: TUI render — gate Other row, blank-before-Submit, row count

**Files:** Modify `crates/atomcode-tuix/src/render/mod.rs` (view struct), `crates/atomcode-tuix/src/render/retained.rs` (`build_user_input_rows`, `user_input_panel_row_count`).

**Interfaces:**
- Consumes: `panel.custom` (Task 2).
- Produces: `UserInputPanelView` gains `pub custom: bool`. Renders the Other row only when `custom`; adds a blank row before the multiple-mode Submit row.

- [ ] **Step 1: Add `custom` to the view struct**

In `render/mod.rs`, add to `UserInputPanelView` (after `custom_text`, before `batch`):

```rust
    /// Whether to render the "Other" free-text row (mirrors UserInputPanel.custom).
    pub custom: bool,
```

Update the 3 test constructions of `UserInputPanelView` in `retained.rs` (grep `custom_text: ` in the test module) to add `custom: true,`, and the batch-render test's `base` closure likewise.

- [ ] **Step 2: Read the current renderer**

Run: `sed -n '2515,2560p' crates/atomcode-tuix/src/render/retained.rs` and read `build_user_input_rows` (the option loop, the Other-row block, and the multiple-mode Submit block).

- [ ] **Step 3: Gate the Other row + add the Submit blank in `build_user_input_rows`**

In the single/multiple arm of `build_user_input_rows`:
- Wrap the entire "Always-appended custom-answer row" block (the `{ let idx = other_index; ... out.push(row); }` block) in `if panel.custom { ... }`.
- Before the multiple-mode Submit block, push one blank spacer row: `if multiple { blank_row(&mut out); /* then the existing Submit row */ }`. The submit-row's `on_cursor` index must use `submit_index = panel.options.len() + panel.custom as usize` (was `other_index + 1`).
- The hint's `n` (navigable count) becomes `panel.options.len() + panel.custom as usize` (was `+ 1`).

- [ ] **Step 4: Match `user_input_panel_row_count`**

In `user_input_panel_row_count`, for single/multiple:
- Change the unconditional `n += 1;` for the custom row to `if panel.custom { n += 1; }`.
- For multiple, the Submit contribution becomes `+2` (blank + submit) instead of `+1`.

Concretely the single/multiple branch becomes:

```rust
            UserInputMode::Single | UserInputMode::Multiple => {
                let mut n = 4; // header chip + blank + question + blank
                for (_, desc) in &panel.options {
                    n += 1;
                    if desc.as_deref().map(|d| !d.trim().is_empty()).unwrap_or(false) {
                        n += 1;
                    }
                }
                if panel.custom {
                    n += 1; // the Other row
                }
                if matches!(panel.mode, UserInputMode::Multiple) {
                    n += 2; // blank spacer + Submit row
                }
                n += 2; // blank + hint
                n
            }
```

- [ ] **Step 5: Wire `custom` into the view construction**

(Deferred to Task 4's event-loop edit, which builds `UserInputPanelView` — but if any `UserInputPanelView { .. }` literal exists in `render/` non-test code, add `custom: panel.custom`.) In this task, only the struct field + tests + renderer change; the production construction is in event_loop (Task 4).

- [ ] **Step 6: Update/extend tests + run**

Update the existing `user_input_panel_renders_all_three_modes` multiple-mode row-count expectation (it gains +1 for the new blank). Add a `custom: false` case asserting the Other row is absent and the invariant holds:

```rust
        // custom == false: no Other row; row_count still matches build.
        let no_custom = crate::render::UserInputPanelView { custom: false, ..view_multiple.clone() };
        assert_eq!(
            r.build_user_input_rows(&no_custom, 78, 80).len(),
            r.user_input_panel_row_count(&no_custom),
            "row_count invariant holds with custom=false"
        );
```

Run: `cargo test -p atomcode-tuix --lib user_input`
Expected: PASS (row-count invariant holds for both `custom` values and the new blank).

- [ ] **Step 7: Commit** (stage only render/ hunks)

```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/render/retained.rs
git commit -m "feat(tuix): render Other row only when custom + blank before Submit

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: TUI events — digit handler + view construction

**Files:** Modify `crates/atomcode-tuix/src/event_loop/mod.rs`.

**Interfaces:**
- Consumes: `panel.custom` (Task 2).
- Produces: number keys can't jump to a non-existent Other row when `custom == false`; the `UserInputPanelView` construction passes `custom`.

- [ ] **Step 1: Gate the digit handler (single + batch)**

In `handle_user_input_key` (single) and `handle_user_input_batch_key` (batch), the digit-key arm does `if idx == p.other_index() { p.cursor = p.other_index(); }`. Guard both with `custom`:

```rust
                if idx < p.options.len() {
                    match p.mode { /* Multiple → toggle_index(idx); _ => cursor = idx */ }
                } else if idx == p.other_index() && p.custom {
                    p.cursor = p.other_index();
                }
```

(i.e. only treat the Nth+1 number as the Other row when `p.custom`; otherwise ignore it.) Adjust the existing `if idx <= p.other_index()` outer guard to `if idx < p.options.len() || (idx == p.other_index() && p.custom)`.

- [ ] **Step 2: Pass `custom` in view construction**

At the `UserInputPanelView { .. }` construction sites in `event_loop/mod.rs` (the single-panel `.map(|p| ...)` and the batch branch), add `custom: p.custom,` (single) and `custom: p.custom,` where `p = &b.questions[idx]` (batch).

- [ ] **Step 3: Build + run the suite**

Run: `cargo build -p atomcode-tuix && cargo test -p atomcode-tuix`
Expected: compiles; full suite green (single-question default `custom=true` unchanged; the multiple-mode blank updated in Task 3's tests).

- [ ] **Step 4: Commit** (stage only your hunks)

```bash
git add -p crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): don't jump to the Other row via number keys when custom=false; pass custom to the view

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: daemon + webui — forward and honor `custom`

**Files:** Modify `crates/atomcode-daemon/src/live_api.rs`, `webui/src/api.ts`, `webui/src/components/UserInputCard.tsx`.

**Interfaces:**
- Consumes: the `custom` field on the request payload (Task 1).
- Produces: the webui hides the Other radio/checkbox + free-text when `custom === false`.

- [ ] **Step 1: daemon — forward `custom` on the single-question event**

In `live_api.rs`, the `LiveWireEvent::UserInputRequest` projection: add a `custom: bool` field to the event (default true) read from `request.payload.get("custom").and_then(Value::as_bool).unwrap_or(true)`. (The batch path already carries `custom` inside each `questions[]` item.)

- [ ] **Step 2: webui types**

In `api.ts`: add `custom?: boolean` to `UserInputQuestion` and to `UserInputRequestEvent`.

- [ ] **Step 3: webui — honor `custom` in `QuestionBody`/`SingleCard`**

In `UserInputCard.tsx`, compute `const showOther = q.custom !== false;` and render the "Other" radio (single) / checkbox (multiple) + the free-text input only when `showOther`. For the single card, `q.custom` comes from `req.custom`; for the batch stepper, from `req.questions[step].custom`.

- [ ] **Step 4: Type-check**

Run: `cd webui && npx tsc --noEmit`
Expected: no errors.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-daemon/src/live_api.rs webui/src/
git commit -m "feat(daemon,webui): forward + honor request_user_input custom flag

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- `custom` field + serde default + schema + description → Task 1. ✓
- TUI state gates Other row (index math) → Task 2. ✓
- Render gates Other row + blank-before-Submit + row count → Task 3. ✓
- Event-loop digit handler + view `custom` → Task 4. ✓
- daemon forward + webui honor → Task 5. ✓
- Row-count invariant for both `custom` values + new blank → Task 3 Steps 4/6. ✓
- Existing multiple-mode row-count test updated for the +1 blank → Task 3 Step 6. ✓
- WIP-staging caution → Global Constraints + `git add -p` in Tasks 2/4. ✓

**Placeholder scan:** Tasks 1-2 carry complete code; 3-5 give exact anchors + the specific gating/edits (they modify large existing functions, so they show the delta not the whole 300-line renderer). No "TBD"/"handle edge cases".

**Type consistency:** `custom: bool` used consistently on `UserInputRequest` (Task 1), `UserInputPanel` (Task 2), `UserInputPanelView` (Task 3), the webui types (Task 5). `submit_index = options.len() + custom as usize` used identically in state (Task 2) and render (Task 3).

---

## Execution Notes

- Tasks 2 & 4 touch `state.rs` / `event_loop/mod.rs`, which may hold unrelated WIP — use `git add -p` and stage only the described hunks.
- Ships **未真机** for the visual effect (Other row hidden on `custom:false`; blank above Submit). Verify by asking a question with `custom:false` (exhaustive options) and a multiple-choice question in a real terminal / webui.
