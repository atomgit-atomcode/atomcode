# Multi-question `request_user_input` (batch form)

**Date:** 2026-07-22
**Status:** Approved, ready for implementation plan
**Scope:** Medium — tool schema + TUI batch UI + webui sequential fallback + daemon endpoint. No kernel change.

## Goal

Let one `request_user_input` tool call pose up to 4 questions answered in a single
interaction: in the TUI the user Tabs between questions, answers each, and submits
them together; in the webui the questions are stepped through one card at a time
and submitted as one batch. This is the structural follow-up to the
[brainstorming → request_user_input persona bridge](2026-07-22-brainstorming-request-user-input-design.md),
finally giving a multi-question interview a proper single-interaction form.

Captured constraints from brainstorming:
- **Scope B**: TUI gets the full multi-question Tab UI; the webui *falls back* to
  stepping through questions one at a time (reusing today's single-question card),
  submitting one batched response at the end. No parallel/form webui UI this round.
- **Partial submit allowed (B)**: the user may submit having answered only some
  questions; unanswered questions come back as `declined`. `Esc` declines the whole
  batch. (Consistent with today's single-question Esc-declines philosophy.)
- **Backward compatible**: extend the existing `request_user_input` tool, do not add
  a new tool.

## Non-goals (deferred)

- A parallel/side-by-side multi-question **form** in the webui (this round: sequential
  stepper only).
- Strong model guidance to "batch questions". The tool description mentions the
  capability ("you may ask up to 4 related questions at once"); whether to batch is
  the model's choice. Not forced against brainstorming's one-at-a-time default.
- More than 4 questions per call (clamped to 4).

## Architecture

The kernel request/response seam passes an opaque `serde_json::Value` in BOTH
directions (`AgentEvent::Request { payload }` / `AgentCommand::Respond { value }`),
so **no kernel change is needed**. The batch shape lives entirely in the tool
payload and the driver response.

Data flow:
```
tool sends {questions:[...]}  →  driver (TUI/webui) collects answers
                              →  driver responds {responses:[...]}
                              →  tool formats per-question result for the model
```

### Unit 1 — Tool layer (`crates/atomcode-capabilities/src/tools/request_user_input.rs`)

- **Schema:** add an optional top-level `questions` array. Each item is the current
  `{ header, question, mode, options }` shape. When `questions` is present and
  non-empty → batch; otherwise the current top-level single-question shape (legacy).
- **Parse:** normalize either shape into an internal `Vec<UserInputRequest>`
  (length `1..=4`; clamp to the first 4 if the model sends more).
- **Payload to driver:** batch → `{ "questions": [ <UserInputRequest>, ... ] }`;
  single (1 question via legacy shape) → keep the current flat
  `{ header, question, mode, options }` so existing driver parsing is untouched.
  (A batch of exactly 1 built from the legacy shape stays legacy on the wire.)
- **Response from driver:** batch → `{ "responses": [ <UserInputResponse>, ... ] }`
  (one per question, in order); single → the current flat `UserInputResponse`.
- **`format_result` (batch):** one line per question, keyed by its `header`, e.g.
  `Q1 (Approvals): User selected "approval request"` / `Q2 (Shape): User answered "…"`
  / `Q3 (Triggers): No answer (declined)`. Single-question formatting is unchanged.
- **Null/decline:** a null response (driver crash / auto-skip) or an all-declined
  batch degrades to the existing "no answer, proceed with best judgment" guidance.

### Unit 2 — TUI batch state + navigation (`crates/atomcode-tuix/src/state.rs`)

- Introduce `UserInputBatch { request_id: u64, questions: Vec<UserInputPanel>, current: usize }`.
  `UserInputPanel` (today's single-question state — cursor, checked, text,
  custom_text, Other row) is **reused verbatim as the per-question state**; its
  `request_id` field moves up to the batch (per-question panels no longer carry it).
- `current` ranges `0..=questions.len()`: `0..questions.len()` are the question
  panels, and `questions.len()` is the **Submit stop**.
- Navigation methods: `next_question()` / `prev_question()` wrap through the Submit
  stop (Tab / Shift+Tab); the current question's own `move_up/down`, `toggle`,
  `push/pop_custom` operate on `questions[current]`.
- `build_batch_response() -> Vec<UserInputResponse>`: maps each panel via the
  existing per-question `build_response()`; a question the user never touched yields
  a `declined` response (partial-submit semantics).
- **N==1 degrades to today's behavior exactly**: no Submit stop, no navigator; `Enter`
  submits immediately (as the current single-question path does). The single-question
  render/handlers stay pixel- and key-identical; only N>1 adds the navigator + Tab +
  Submit stop.

### Unit 3 — TUI rendering (`crates/atomcode-tuix/src/render/`)

- A `UserInputBatchView` (or an extended view) that, for N>1, renders a top navigator
  row `Question {current+1}/{N}` with per-question status glyphs (answered `✓` /
  untouched `○`, ASCII-downgraded via the existing glyph backstop), then the current
  question's rows via the **existing `build_user_input_rows` per-question logic**, then
  a Submit row when `current == questions.len()`, then a hint mentioning Tab.
- For N==1 the output is byte-identical to today (no navigator, no batch chrome).

### Unit 4 — TUI event handling (`crates/atomcode-tuix/src/event_loop/mod.rs`)

- `handle_user_input_key` for a batch: `Tab`/`Shift+Tab` → `next_question`/`prev_question`;
  `↑↓`, `Space`, digit keys, char, `Backspace` act on `questions[current]` exactly as
  today; `Enter` on a single-mode question selects and advances to the next question
  (or to the Submit stop if it was the last); `Enter` on the Submit stop builds
  `build_batch_response()` and delivers it; `Esc` declines the whole batch (all
  questions → declined). N==1 keeps today's Enter-submits-immediately behavior.
- Request parsing: when the payload has a non-empty `questions` array, build a
  `UserInputBatch`; otherwise build today's single `UserInputPanel`.
- `deliver_user_input` gains a batch path that responds `{ "responses": [...] }`.

### Unit 5 — webui sequential fallback (`webui/src/components/UserInputCard.tsx`, `webui/src/api.ts`)

- On a `user_input_request` SSE event carrying `questions`, the card steps through
  them **one at a time reusing today's single-question rendering**: answer Q1 → stash
  locally → show Q2 → … → on the last question, POST the accumulated batch once as
  `{ request_id, responses: [ {declined,selected,text}, ... ] }`.
- A single question (legacy payload) posts the current flat body unchanged.
- The kernel awaits ONE response per `request_id`, so the webui must accumulate
  locally and send exactly one final POST.

### Unit 6 — daemon endpoint (`crates/atomcode-daemon/src/live_api.rs`)

- `UserInputAnswerReq` gains an optional `responses: Vec<UserInputResponse-shape>`.
  `live_user_input`: when `responses` is present, respond
  `{ "responses": [...] }`; otherwise the current flat `{ declined, selected, text }`.
- The `AgentEvent::Request` → `LiveWireEvent::UserInputRequest` projection also
  forwards the `questions` array when present (so the webui receives it).

## Testing

- **Tool:** parse both shapes; clamp >4 to 4; `format_result` batch output (per-question
  lines keyed by header, including a declined question); batch response
  deserialization; single-question path unchanged.
- **TUI state:** `UserInputBatch` navigation (Tab wrap through Submit stop, prev/next
  bounds), `build_batch_response` (untouched question → declined; mixed answered/
  unanswered), N==1 degradation equals the single-question path.
- **TUI events:** Tab/Shift+Tab move between questions; Enter advances then submits on
  the Submit stop; Esc declines all; digit/space/char route to the current question.
- **webui:** stepper accumulates per-question answers and fires exactly one batched
  POST on the last question; single-question path posts the flat body.
- Run existing `request_user_input` / tuix panel tests — no regression on the
  single-question path.

## Risks / notes

- **Single-question regression risk** is the main hazard (the existing panel is widely
  used). Mitigation: N==1 shares the same render/handler code and is asserted
  byte/key-identical by a dedicated test; the batch chrome only appears for N>1.
- **Wire compatibility**: batch adds a new payload/response shape; a driver that
  doesn't understand `questions` would fail to parse — acceptable because both drivers
  (TUI + webui) are updated in this same change, and the legacy single shape is
  preserved for single-question calls.
- Whether the model actually batches questions is out of scope (its choice); this
  change only provides the capability + a one-line tool-description mention.
