# Multi-question `request_user_input` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let one `request_user_input` call pose up to 4 questions answered in a single interaction — Tab-navigated form in the TUI, sequential stepper in the webui — submitted as one batched response.

**Architecture:** No kernel change (the request/response payload is an opaque `serde_json::Value`). The tool sends `{questions:[...]}`, drivers collect answers and respond `{responses:[...]}`, the tool formats one line per question. The TUI reuses `UserInputPanel` as per-question state inside a new `UserInputBatch`; the webui steps through questions reusing its single-question card and posts one batch at the end.

**Tech Stack:** Rust (`atomcode-capabilities`, `atomcode-tuix`, `atomcode-daemon`), React/TS (`webui`), `cargo test`.

## Global Constraints

- **Backward compatible.** A call with the legacy flat shape (`header/question/mode/options`, no `questions`) keeps the exact current single-question wire, UI, and result. Only a non-empty `questions` array activates the batch path.
- **Max 4 questions.** Clamp to the first 4 (`MAX_QUESTIONS = 4`).
- **Partial submit (B).** A question the user never answered comes back `declined`. `Esc` declines the whole batch.
- **Scope B.** TUI gets the full Tab form; webui steps through questions one card at a time and posts one batched response. No parallel webui form this round.
- **N==1 must not regress.** A single-question interaction (whether legacy flat or a 1-element `questions`) renders and behaves exactly as today.
- Work on branch `release/v5.0.1`. Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

---

## File Structure

- `crates/atomcode-capabilities/src/tools/request_user_input.rs` — tool: batch parse, batch format, schema, execute. (Task 1)
- `crates/atomcode-tuix/src/state.rs` — `UserInputBatch` wrapper over `UserInputPanel`. (Task 2)
- `crates/atomcode-tuix/src/render/mod.rs` + `render/retained.rs` — batch navigator + reuse per-question rows. (Task 3)
- `crates/atomcode-tuix/src/event_loop/mod.rs` — Tab/Shift+Tab, request parsing, batch deliver. (Task 4)
- `crates/atomcode-daemon/src/live_api.rs` — batched response body + `questions` projection. (Task 5)
- `webui/src/components/UserInputCard.tsx` + `webui/src/api.ts` — sequential stepper + batched POST. (Task 6)

---

### Task 1: Tool layer — batch parse, format, execute

**Files:**
- Modify: `crates/atomcode-capabilities/src/tools/request_user_input.rs`
- Test: same file (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `UserInputRequest`, `UserInputResponse`, `UserInputMode`, `parse_args`, `format_result`, `ok_result`/`err_result`/`null_result`.
- Produces: `pub const MAX_QUESTIONS: usize`; `pub fn parse_batch(args: &str) -> Result<(Vec<UserInputRequest>, bool), String>` (bool = is_batch); `pub fn format_batch_result(reqs: &[UserInputRequest], resps: &[UserInputResponse]) -> ToolResult`. Task 4/5 rely on the wire shapes: request `{ "questions": [UserInputRequest,...] }`, response `{ "responses": [UserInputResponse,...] }`.

- [ ] **Step 1: Write failing tests**

Add to `mod tests`:

```rust
    #[test]
    fn parse_batch_reads_questions_array_and_clamps_to_four() {
        let args = r#"{"questions":[
            {"header":"A","question":"Q1?","mode":"single","options":[{"label":"x"}]},
            {"header":"B","question":"Q2?","mode":"text"},
            {"header":"C","question":"Q3?","mode":"text"},
            {"header":"D","question":"Q4?","mode":"text"},
            {"header":"E","question":"Q5?","mode":"text"}
        ]}"#;
        let (reqs, is_batch) = parse_batch(args).unwrap();
        assert!(is_batch);
        assert_eq!(reqs.len(), 4, "clamped to MAX_QUESTIONS");
        assert_eq!(reqs[0].header, "A");
    }

    #[test]
    fn parse_batch_falls_back_to_single_legacy_shape() {
        let (reqs, is_batch) =
            parse_batch(r#"{"header":"H","question":"Q?","mode":"text"}"#).unwrap();
        assert!(!is_batch);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].mode, UserInputMode::Text);
    }

    #[test]
    fn parse_batch_validates_each_question_options() {
        let args = r#"{"questions":[{"header":"A","question":"Q?","mode":"single","options":[]}]}"#;
        assert!(parse_batch(args).is_err(), "choice question needs options");
    }

    #[test]
    fn format_batch_keys_each_line_by_header_and_declines_untouched() {
        let reqs = vec![
            UserInputRequest { header: "Auth".into(), question: "?".into(), mode: UserInputMode::Single, options: vec![UserInputOption{label:"OAuth".into(),description:None}] },
            UserInputRequest { header: "Note".into(), question: "?".into(), mode: UserInputMode::Text, options: vec![] },
        ];
        let resps = vec![
            UserInputResponse { declined: false, selected: vec!["OAuth".into()], text: None },
            UserInputResponse::declined(),
        ];
        let out = format_batch_result(&reqs, &resps).content;
        assert_eq!(out, "Q1 (Auth): User selected: \"OAuth\"\nQ2 (Note): No answer (declined).");
    }

    #[test]
    fn format_batch_all_declined_is_the_single_no_answer_guidance() {
        let reqs = vec![UserInputRequest { header: "A".into(), question: "?".into(), mode: UserInputMode::Text, options: vec![] }];
        let out = format_batch_result(&reqs, &[UserInputResponse::declined()]);
        assert!(!out.is_error);
        assert!(out.content.starts_with("No answer was provided."));
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-capabilities --lib request_user_input`
Expected: FAIL to compile (`parse_batch` / `format_batch_result` / `MAX_QUESTIONS` undefined).

- [ ] **Step 3: Implement `MAX_QUESTIONS`, `validate_question`, `parse_batch`, `format_batch_result`**

Refactor `parse_args` to share validation, and add the batch functions. Replace the existing `parse_args` (lines 55-67) with:

```rust
/// Max questions a single batch may pose.
pub const MAX_QUESTIONS: usize = 4;

fn validate_question(req: &UserInputRequest) -> Result<(), String> {
    if matches!(req.mode, UserInputMode::Single | UserInputMode::Multiple) && req.options.is_empty()
    {
        return Err(
            "request_user_input: single/multiple mode requires a non-empty `options` array".into(),
        );
    }
    Ok(())
}

/// Parse raw tool args into a `UserInputRequest`. Rejects choice modes with no options.
/// Returns a human message on failure (never panics).
pub fn parse_args(args: &str) -> Result<UserInputRequest, String> {
    let req: UserInputRequest = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    validate_question(&req)?;
    Ok(req)
}

/// Parse args into 1..=`MAX_QUESTIONS` questions. Accepts a `{ "questions": [...] }`
/// array (batch) or the flat single-question shape (legacy). The bool is `is_batch`
/// — the caller uses it to pick the wire shape. Clamps a batch to `MAX_QUESTIONS`.
pub fn parse_batch(args: &str) -> Result<(Vec<UserInputRequest>, bool), String> {
    let val: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| format!("invalid request_user_input arguments: {e}"))?;
    if let Some(qs) = val.get("questions").and_then(serde_json::Value::as_array) {
        if qs.is_empty() {
            return Err("request_user_input: `questions` must be a non-empty array".into());
        }
        let mut out = Vec::new();
        for q in qs.iter().take(MAX_QUESTIONS) {
            let req: UserInputRequest = serde_json::from_value(q.clone())
                .map_err(|e| format!("invalid question in `questions`: {e}"))?;
            validate_question(&req)?;
            out.push(req);
        }
        Ok((out, true))
    } else {
        Ok((vec![parse_args(args)?], false))
    }
}

/// Map one question's response to its answer clause (shared by single + batch).
fn answer_clause(resp: &UserInputResponse) -> String {
    if resp.declined {
        return "No answer (declined).".to_string();
    }
    if let Some(t) = &resp.text {
        return format!("User answered: {t:?}");
    }
    if resp.selected.is_empty() {
        return "User selected nothing.".to_string();
    }
    let joined = resp
        .selected
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("User selected: {joined}")
}

/// Format a batch of answers, one line per question keyed by its `header`. When every
/// question was declined, degrade to the same "no answer" guidance a single decline gives.
pub fn format_batch_result(reqs: &[UserInputRequest], resps: &[UserInputResponse]) -> ToolResult {
    if resps.iter().all(|r| r.declined) && resps.len() >= reqs.len() {
        return ok_result(
            "No answer was provided. Proceed with your own best judgment; only ask again if you \
             are truly blocked.",
        );
    }
    let lines: Vec<String> = reqs
        .iter()
        .enumerate()
        .map(|(i, req)| {
            let clause = resps
                .get(i)
                .map(answer_clause)
                .unwrap_or_else(|| "No answer (declined).".to_string());
            format!("Q{} ({}): {}", i + 1, req.header, clause)
        })
        .collect();
    ok_result(lines.join("\n"))
}
```

Note: `format_result` (single) can optionally be simplified to reuse `answer_clause`, but leave it as-is to avoid churn — its exact strings are asserted by existing tests.

- [ ] **Step 4: Rewrite `execute` to route batch vs single**

Replace `execute` (lines 156-173) with:

```rust
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let (reqs, is_batch) = match parse_batch(args) {
            Ok(x) => x,
            Err(e) => return err_result(e),
        };
        if !is_batch {
            // Legacy single-question path — wire + result unchanged.
            let payload = match serde_json::to_value(&reqs[0]) {
                Ok(v) => v,
                Err(e) => return err_result(format!("request_user_input: serialize failed: {e}")),
            };
            let resp_val = ctx.request(REQUEST_USER_INPUT_KIND, payload).await;
            if resp_val.is_null() {
                return null_result();
            }
            return match serde_json::from_value::<UserInputResponse>(resp_val) {
                Ok(resp) => format_result(&resp),
                Err(_) => format_result(&UserInputResponse::declined()),
            };
        }
        // Batch path.
        let payload = serde_json::json!({ "questions": reqs });
        let resp_val = ctx.request(REQUEST_USER_INPUT_KIND, payload).await;
        if resp_val.is_null() {
            return null_result();
        }
        let resps: Vec<UserInputResponse> = resp_val
            .get("responses")
            .and_then(|r| serde_json::from_value::<Vec<UserInputResponse>>(r.clone()).ok())
            .unwrap_or_default();
        format_batch_result(&reqs, &resps)
    }
```

- [ ] **Step 5: Extend the schema + description for `questions`**

Replace `parameters_schema` (lines 132-154) so it adds an optional `questions` array and drops top-level `required` (a batch call has no top-level `header`); update `description` to mention batching. New `description`:

```rust
    fn description(&self) -> &str {
        "Ask the user structured question(s) and wait for their answer before continuing. \
         Use ONLY for decisions that are genuinely the user's to make — a preference, a \
         confirmation, a choice between approaches — NOT for anything you can decide, look \
         up, or verify yourself. For ONE question, set `header`, `question`, `mode` \
         (\"single\"=pick one, \"multiple\"=pick any, \"text\"=free-form) and `options` \
         (non-empty for single/multiple). To ask up to 4 related questions answered in ONE \
         interaction, pass a `questions` array of those same objects instead. Keep each \
         `header` short (a few words)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let question = serde_json::json!({
            "type": "object",
            "required": ["header", "question", "mode"],
            "properties": {
                "header": {"type": "string", "description": "Very short label (a few words)."},
                "question": {"type": "string", "description": "One clear sentence, ideally ending in '?'."},
                "mode": {"type": "string", "enum": ["single", "multiple", "text"]},
                "options": {
                    "type": "array",
                    "description": "Choices for single/multiple; omit for text.",
                    "items": {
                        "type": "object",
                        "required": ["label"],
                        "properties": {
                            "label": {"type": "string"},
                            "description": {"type": "string"}
                        }
                    }
                }
            }
        });
        serde_json::json!({
            "type": "object",
            "properties": {
                "header": question["properties"]["header"],
                "question": question["properties"]["question"],
                "mode": question["properties"]["mode"],
                "options": question["properties"]["options"],
                "questions": {
                    "type": "array",
                    "description": "Up to 4 questions answered in one interaction. Provide EITHER top-level header/question/mode/options for a single question, OR this array.",
                    "maxItems": 4,
                    "items": question
                }
            }
        })
    }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p atomcode-capabilities --lib request_user_input`
Expected: PASS — new batch tests + all existing single-question tests (unchanged strings).

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-capabilities/src/tools/request_user_input.rs
git commit -m "feat(request_user_input): batch questions in the tool layer

Accept an optional questions[] array (max 4) alongside the legacy single-question
shape; send {questions:[...]} and read back {responses:[...]}; format one result
line per question keyed by header. Single-question wire + result unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: TUI batch state (`UserInputBatch`)

**Files:**
- Modify: `crates/atomcode-tuix/src/state.rs` (add `UserInputBatch` after `UserInputPanel`, ~line 310)
- Test: `crates/atomcode-tuix/src/state.rs` (a `#[cfg(test)] mod` if one exists, else add one)

**Interfaces:**
- Consumes: existing `UserInputPanel` (per-question state, reused verbatim) and `UserInputRequest`/`UserInputResponse`.
- Produces: `pub struct UserInputBatch { pub request_id: u64, pub questions: Vec<UserInputPanel>, pub current: usize }` with `new`, `is_multi`, `submit_stop`, `on_submit_stop`, `next_question`, `prev_question`, `is_answered`, `build_batch_response`. Task 3 reads `questions`/`current`/`is_answered`; Task 4 calls navigation + `build_batch_response`.

- [ ] **Step 1: Write failing tests**

Add near the bottom of `state.rs` (adjust `mod tests`/imports to the file's convention):

```rust
#[cfg(test)]
mod user_input_batch_tests {
    use super::*;
    use atomcode_capabilities::tools::request_user_input::{
        UserInputMode, UserInputOption, UserInputRequest,
    };

    fn text_q(h: &str) -> UserInputRequest {
        UserInputRequest { header: h.into(), question: "?".into(), mode: UserInputMode::Text, options: vec![] }
    }
    fn single_q(h: &str) -> UserInputRequest {
        UserInputRequest { header: h.into(), question: "?".into(), mode: UserInputMode::Single,
            options: vec![UserInputOption { label: "x".into(), description: None }] }
    }

    #[test]
    fn tab_wraps_through_submit_stop() {
        let mut b = UserInputBatch::new(7, &[text_q("a"), text_q("b")]);
        assert_eq!(b.current, 0);
        assert_eq!(b.submit_stop(), 2);
        b.next_question(); assert_eq!(b.current, 1);
        b.next_question(); assert_eq!(b.current, 2); // submit stop
        assert!(b.on_submit_stop());
        b.next_question(); assert_eq!(b.current, 0); // wrap
        b.prev_question(); assert_eq!(b.current, 2); // wrap back to submit stop
    }

    #[test]
    fn build_batch_response_declines_untouched_questions() {
        let mut b = UserInputBatch::new(1, &[single_q("a"), text_q("b")]);
        // Answer q0 by moving its cursor onto the concrete option (cursor 0 already is it).
        b.questions[0].select_current_option();
        let resps = b.build_batch_response();
        assert_eq!(resps.len(), 2);
        assert!(!resps[0].declined, "answered question 0");
        assert_eq!(resps[0].selected, vec!["x".to_string()]);
        assert!(resps[1].declined, "untouched text question 1 → declined");
    }

    #[test]
    fn is_answered_tracks_content() {
        let mut b = UserInputBatch::new(1, &[text_q("a")]);
        assert!(!b.is_answered(0), "empty text → not answered");
        b.questions[0].text.push_str("hi");
        assert!(b.is_answered(0));
    }

    #[test]
    fn single_question_batch_is_not_multi() {
        let b = UserInputBatch::new(1, &[text_q("only")]);
        assert!(!b.is_multi());
        assert_eq!(b.submit_stop(), 1);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-tuix --lib user_input_batch`
Expected: FAIL to compile (`UserInputBatch` undefined).

- [ ] **Step 3: Implement `UserInputBatch`**

Add after `impl UserInputPanel { ... }` (after line 310) in `state.rs`:

```rust
/// A batch of 1..=4 questions answered in one interaction. Wraps per-question
/// `UserInputPanel`s; `request_id` lives here (the panels' own `request_id` is unused
/// in a batch). `current` ranges `0..questions.len()` (question panels) plus
/// `questions.len()` (the Submit stop that `Tab` cycles to).
pub struct UserInputBatch {
    pub request_id: u64,
    pub questions: Vec<UserInputPanel>,
    pub current: usize,
}

impl UserInputBatch {
    pub fn new(
        request_id: u64,
        reqs: &[atomcode_capabilities::tools::request_user_input::UserInputRequest],
    ) -> Self {
        let questions = reqs.iter().map(|r| UserInputPanel::new(request_id, r)).collect();
        Self { request_id, questions, current: 0 }
    }

    /// More than one question → render the navigator + Tab/Submit chrome.
    pub fn is_multi(&self) -> bool {
        self.questions.len() > 1
    }

    /// The Submit stop index (one past the last question).
    pub fn submit_stop(&self) -> usize {
        self.questions.len()
    }

    pub fn on_submit_stop(&self) -> bool {
        self.current == self.submit_stop()
    }

    /// `Tab`: next question, wrapping through the Submit stop back to the first.
    pub fn next_question(&mut self) {
        self.current = if self.current >= self.submit_stop() { 0 } else { self.current + 1 };
    }

    /// `Shift+Tab`: previous question, wrapping to the Submit stop.
    pub fn prev_question(&mut self) {
        self.current = if self.current == 0 { self.submit_stop() } else { self.current - 1 };
    }

    /// Whether question `i` has real content (used for the ✓/○ navigator marker).
    pub fn is_answered(&self, i: usize) -> bool {
        self.questions.get(i).is_some_and(Self::panel_answered)
    }

    /// One response per question, in order. A question with no real content becomes
    /// `declined` (partial-submit semantics).
    pub fn build_batch_response(
        &self,
    ) -> Vec<atomcode_capabilities::tools::request_user_input::UserInputResponse> {
        use atomcode_capabilities::tools::request_user_input::UserInputResponse;
        self.questions
            .iter()
            .map(|p| {
                if Self::panel_answered(p) {
                    p.build_response().unwrap_or_else(UserInputResponse::declined)
                } else {
                    UserInputResponse::declined()
                }
            })
            .collect()
    }

    /// A panel counts as answered when it builds a response with a non-empty selection
    /// or non-blank text. (Text mode's `build_response` is always `Some`, possibly empty.)
    fn panel_answered(p: &UserInputPanel) -> bool {
        match p.build_response() {
            Some(r) => {
                !r.selected.is_empty()
                    || r.text.as_deref().map(|t| !t.trim().is_empty()).unwrap_or(false)
            }
            None => false,
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p atomcode-tuix --lib user_input_batch`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/state.rs
git commit -m "feat(tuix): UserInputBatch — per-question state + Tab/submit navigation

Wraps UserInputPanel as per-question state with a current index that cycles
through the questions and a Submit stop. build_batch_response yields one response
per question, declining untouched ones (partial submit).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: TUI rendering — batch navigator + reuse per-question rows

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs` (view struct ~571-591), `crates/atomcode-tuix/src/render/retained.rs` (`user_input_panel_row_count` ~2519, `build_user_input_rows` ~2560)
- Modify: wherever the panel view is produced from state (grep `UserInputPanelView` construction)

**Interfaces:**
- Consumes: `UserInputBatch` (Task 2) from `state`.
- Produces: batch rendering — for `is_multi()`, a leading navigator row `Question {current+1}/{N}` with per-question `✓`/`○` markers, the current question's existing rows, a Submit row when `on_submit_stop()`, and a Tab hint. For N==1, byte-identical to today.

- [ ] **Step 1: Read the current single-question renderer**

Run: `sed -n '2519,2620p' crates/atomcode-tuix/src/render/retained.rs` and read `UserInputPanelView` at `render/mod.rs:571-591`. Note how `build_user_input_rows` emits header/question/option/Other/Submit/hint rows and how the caller builds the view from `state.user_input_panel`.

- [ ] **Step 2: Add a batch view + navigator (no behavior change for N==1)**

In `render/mod.rs`, add a `UserInputBatchView` that carries `current: usize`, `total: usize`, `answered: Vec<bool>`, and the current question's `UserInputPanelView`. In `retained.rs`, add `build_user_input_batch_rows(&self, view: &UserInputBatchView) -> Vec<Vec<Cell>>` that:
  - when `total > 1`: pushes a navigator row `Question {current+1}/{total}` followed by per-question `✓`(answered)/`○`(not) markers (use the existing glyph-downgrade path so non-unicode terminals get `x`/`o`), then delegates to the existing per-question row builder for the current `UserInputPanelView`, then (when the cursor is on the Submit stop) a `提交 / Submit` row, then a hint row that includes `Tab 切换问题`;
  - when `total == 1`: calls the existing `build_user_input_rows` unchanged (byte-identical output).
  Extract the current per-question body of `build_user_input_rows` into a shared helper if needed so both paths share it (DRY) — do NOT duplicate the option-row logic.

- [ ] **Step 3: Wire the view construction from `UserInputBatch`**

At the site that builds `UserInputPanelView` from `state.user_input_panel`, add the parallel construction from `state.user_input_batch` (added in Task 4): build the current question's `UserInputPanelView` from `batch.questions[batch.current]` (when `current < questions.len()`), set `answered[i] = batch.is_answered(i)`, and mark `on_submit_stop`.

- [ ] **Step 4: Row-count test + manual render check**

Add a unit test asserting `build_user_input_batch_rows` for a 1-question batch produces the same rows as `build_user_input_rows` for that question (N==1 parity), and for a 2-question batch includes a row containing `Question 1/2`. Run: `cargo test -p atomcode-tuix --lib user_input`. Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/
git commit -m "feat(tuix): render multi-question batch navigator (N==1 unchanged)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: TUI events — Tab nav, request parsing, batch deliver

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs` — `handle_user_input_key` (~10932), request parsing (~11920), `deliver_user_input` (~10106)
- Modify: `crates/atomcode-tuix/src/state.rs` — add `pub user_input_batch: Option<UserInputBatch>` to the app state struct (next to `user_input_panel`)

**Interfaces:**
- Consumes: `UserInputBatch` (Task 2).
- Produces: batch key handling + a `deliver_user_input_batch` that responds `serde_json::json!({ "responses": batch.build_batch_response() })` via the same `DriverCommand::Respond`/`native_live::respond` path as `deliver_user_input`.

- [ ] **Step 1: Read the current handlers**

Read `handle_user_input_key` (~10932-11078), `deliver_user_input` (~10106-10127), and the request-parsing arm (~11920-11930).

- [ ] **Step 2: Parse a batch request**

In the `REQUEST_USER_INPUT_KIND` arm, before the single-question `from_value::<UserInputRequest>`, try a batch: if `request.payload.get("questions")` is a non-empty array, parse `Vec<UserInputRequest>`, set `state.user_input_batch = Some(UserInputBatch::new(request.id, &reqs))`, `state.phase = UiPhase::UserInput`, redraw, return. Otherwise fall through to today's single-question `UserInputPanel` path unchanged.

- [ ] **Step 3: Handle batch keys**

In `handle_user_input_key`, when `state.user_input_batch.is_some()`, branch to batch handling:
  - `Esc` / `Ctrl+C` → `deliver_user_input_batch` with all-declined (build a `Vec` of `UserInputResponse::declined()` sized to the questions) + clear; matches single Esc semantics.
  - `Tab` → `batch.next_question()`; `BackTab`/`Shift+Tab` → `batch.prev_question()`.
  - On the Submit stop (`batch.on_submit_stop()`): `Enter` → `deliver_user_input_batch(batch.build_batch_response())` + clear.
  - On a question (`current < questions.len()`): route `↑↓`, `Space`, digit, char, `Backspace` to `batch.questions[current]` using the SAME logic as the single panel; `Enter` on a single-mode question → advance (`next_question()`); `Enter` on a multiple-mode option → toggle (unchanged); text mode Enter → advance.
  - Keep the existing single-panel path for `state.user_input_panel.is_some()` untouched (N==1 legacy still uses `user_input_panel`, so its behavior is literally unchanged).

- [ ] **Step 4: Batch deliver**

Add `deliver_user_input_batch(ctx, request_id, resps: Vec<UserInputResponse>)` mirroring `deliver_user_input`, responding `serde_json::json!({ "responses": resps })`.

- [ ] **Step 5: Build + a state-machine test where feasible**

Run: `cargo build -p atomcode-tuix` and `cargo test -p atomcode-tuix`. Expected: compiles, suite green. Add a test for the pure key→navigation transition if the handler exposes a pure helper (mirror `user_input_response_for`); otherwise rely on the Task 2 state tests + build.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/ crates/atomcode-tuix/src/state.rs
git commit -m "feat(tuix): Tab-navigated batch input — parse, keys, batched deliver

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Daemon — batched response body + `questions` projection

**Files:**
- Modify: `crates/atomcode-daemon/src/live_api.rs` — `UserInputAnswerReq` (~1752), `live_user_input` (~1767), the `LiveWireEvent::UserInputRequest` projection (~954)

**Interfaces:**
- Consumes: the same wire shapes as Task 1 (request `{questions:[...]}`, response `{responses:[...]}`).
- Produces: the daemon accepts `{ request_id, responses: [...] }` and forwards the `questions` array to the webui.

- [ ] **Step 1: Accept a batched response**

Add `#[serde(default)] pub responses: Option<serde_json::Value>` to `UserInputAnswerReq`. In `live_user_input`, when `responses` is `Some`, respond `serde_json::json!({ "responses": responses })`; else keep the current flat `{ declined, selected, text }`.

- [ ] **Step 2: Forward `questions` to the webui**

In the `REQUEST_USER_INPUT_KIND` projection, when `payload.get("questions")` is present, include it on `LiveWireEvent::UserInputRequest` (add a `questions: Option<Value>` field to that event, `None` for single). The webui uses it to detect a batch.

- [ ] **Step 3: Build + test**

Run: `cargo build -p atomcode-daemon` and `cargo test -p atomcode-daemon`. Expected: compiles, green. Add a small test that a `UserInputAnswerReq` with `responses` deserializes and produces the `{responses:...}` value if the endpoint has a testable helper; otherwise build-only.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-daemon/src/live_api.rs
git commit -m "feat(daemon): batched user-input response + questions projection

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: WebUI — sequential stepper + batched POST

**Files:**
- Modify: `webui/src/api.ts` — `UserInputRequestEvent` (add `questions?`), `postLiveUserInput` (accept `responses`)
- Modify: `webui/src/components/UserInputCard.tsx` — step through questions, accumulate, post once

**Interfaces:**
- Consumes: the SSE `user_input_request` event (now carrying `questions?`).
- Produces: one POST `{ request_id, responses: [...] }` for a batch; the flat body for a single question.

- [ ] **Step 1: Types**

In `api.ts`: add `questions?: { header: string; question: string; mode: 'single'|'multiple'|'text'; options: {label:string;description?:string}[] }[]` to `UserInputRequestEvent`; widen `postLiveUserInput`'s body to `{ request_id: number } & ({ declined: boolean; selected: string[]; text: string|null } | { responses: {declined:boolean;selected:string[];text:string|null}[] })`.

- [ ] **Step 2: Stepper in `UserInputCard`**

When `event.questions` is present and length > 1: keep local state `stepIndex` and `answers: Response[]`. Render the current question (reuse the existing single-question rendering by driving it from `questions[stepIndex]` instead of the top-level fields). "Next" (or submit-on-last) pushes the current answer into `answers` and advances; on the last question, POST `{ request_id, responses: answers }`. Show `Question {stepIndex+1}/{n}` and a Back control. Skip → push a declined response and advance (partial submit); Skip-all on the first affordance posts all-declined. Single question (no `questions`) keeps today's flat POST.

- [ ] **Step 3: Type-check the webui**

Run: `cd webui && npx tsc --noEmit` (dist is gitignored; only `src` is committed and tsc must pass).
Expected: no type errors.

- [ ] **Step 4: Commit**

```bash
git add webui/src/
git commit -m "feat(webui): sequential multi-question stepper -> one batched answer

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage:**
- Unit 1 tool (schema/parse/format/response) → Task 1. ✓
- Unit 2 TUI batch state → Task 2. ✓
- Unit 3 TUI render navigator (N>1) / N==1 unchanged → Task 3. ✓
- Unit 4 TUI events (Tab/Shift+Tab, parse, batch deliver, Esc-declines-all) → Task 4. ✓
- Unit 5 daemon (batched response + questions projection) → Task 5. ✓
- Unit 6 webui sequential stepper + one batched POST → Task 6. ✓
- Backward compat / N==1 no-regression → Task 1 legacy path, Task 3 Step 2 (N==1 byte-identical), Task 4 (single path via `user_input_panel` untouched). ✓
- Partial submit (untouched → declined) → Task 2 `build_batch_response` + Task 6 skip. ✓
- Max 4 clamp → Task 1 `parse_batch`. ✓

**Placeholder scan:** Tasks 1-2 carry complete code. Tasks 3-6 are integration into large existing functions (300-line renderer, the event handler, a React component); each step gives the exact anchors, the specific new code/shape, and a concrete verify command. No "TBD"/"handle edge cases" — each names the exact rows/keys/fields to add. The read-first steps (3.1, 4.1) are deliberate: the executor reads the current large function before editing it rather than the plan reproducing hundreds of unchanged lines.

**Type consistency:** wire shapes are consistent across tasks — request `{questions:[UserInputRequest]}`, response `{responses:[UserInputResponse]}`. `UserInputBatch { request_id, questions, current }` defined in Task 2 and consumed by Tasks 3/4 with those field names. `format_batch_result(reqs, resps)` / `parse_batch → (Vec, bool)` used consistently.

---

## Execution Notes

- Tasks 1-2 are pure Rust units with full TDD. Tasks 3-6 integrate into existing large files; each starts by reading the current function.
- Order matters for not breaking the drivers: land Tasks 1-5 (tool + both driver ends) before the tool is exercised with a real batch. Since the model only emits `questions` once the new schema is deployed, and all driver ends ship together, there is no intermediate broken state.
- After merge this ships **未真机** for the batch UX — verify by asking deepseek/GLM a design question that surfaces several choices and confirming one Tab-navigated panel (TUI) / sequential stepper (webui) with a single combined answer.
- `webui/dist` is gitignored — commit only `webui/src` and ensure `tsc --noEmit` passes.
