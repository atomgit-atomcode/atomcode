# Goal pending questions implementation plan

**Goal:** ordinary questions must not hold an unattended goal indefinitely. A missing answer is never consent, a selected option, or an approval.

**Architecture:** CodingRuntime owns the foreground deadline, request routing, generation/session fences and goal continuation. The existing native event log owns the question receipt, admitted human reply and cancellation facts. No new session schema, sidecar, agent owner, kernel business command, or approval fallback is introduced.

**Baseline:** started on release/v5.2.4 cdb0bcdf0. The shared checkout advanced to e3186e06e during work; unrelated search/status/throughput changes and untracked user artifacts are preserved.

**Tech stack:** Rust/Tokio, existing harness question and ToolContext request seams, native session facts, existing daemon live wire, Preact WebUI.

## Behavior

- During an active goal, an ordinary question from either supported question tool waits 120 seconds, then returns a typed pending receipt. The original tool result stays immutable. Requests need host-authored tool-call correlation; generic interactive wizards retain their existing wait.
- Approvals and authorization-bearing seam questions keep their existing decision path. Timers never select an answer or grant permission. Delegated ordinary questions return a model-visible instruction to report to the parent instead of waiting on a human the child cannot own.
- At most eight questions are retained. Oversized question payloads above 16 KiB fail explicitly; limit errors do not manufacture answers or strand new indefinite waits.
- A goal round with pending questions and no independent tool activity pauses before another evaluation/main replay. This guards question-only/talking loops; it does not certify that every other tool call made useful progress.
- `/goal questions` lists current unanswered questions and `/goal answer <id> <text>` replies. TUI history describes the historical wait ending and points to the current list, rather than presenting a permanently live historical reply link. A continued live WebUI card survives normal idle until its correlated resolution.

## Ownership and persistence

1. The wire/result helpers live beside `request_user_input`. Harness `ask_user` receives a typed answered/unanswered/failed/pending outcome without calling pending a human answer. Host-generated call ids correlate tool termination, so a shorter driver wait cannot be resurrected by a late goal timer.
2. Runtime retains only transient live routing. New late request ids are JavaScript-safe integers. The old foreground id remains an alias while needed to accept a reply racing the deadline. Duplicate or already queued replies are refused.
3. A late reply uses the existing runtime Submit path with captured generation and session binding and normal image preprocessing. The actual human words remain the UserMessage. Internal per-input context carries the correlation and bounded historical question as untrusted data, so a compacted conversation can still understand the answer without promoting model-authored question text into human instructions.
4. Context and human message are one contextual input. Replay settles the question only when both continuation context and the same-turn UserMessage have committed. A context-only prefix remains pending. A NotSent receipt is not successful delivery. Queued correlation remains available until native admission, including cancellation and callbacks overtaken by a fast committed snapshot.
5. Explicit cancellation closes questions through an existing native Reminder fact using the existing commit seam. Cancellation cannot discard its own withdrawal when interrupted work is omitted. Normal shutdown alone preserves pending questions for restart. Explicit rewind reuses the kernel projection; compaction does not withdraw an unanswered question.
6. Replay reads the live root's existing event log. It does not reread arbitrary files or use model context as a second session authority. Projection avoids duplicating unrelated tool output and permits provider call ids to be reused after their previous result completed. Pending-only rounds do not repeatedly rescan the full log; reconciliation is needed when a reply is queued/admitted.

## Affected surfaces

- `atomcode-capabilities`: typed pending receipt, ordinary-question result/projection helpers and host call correlation.
- `atomcode-harness`: compatible default question outcome method, `ask_user`, handle request provenance/correlation, unchanged approval decisions.
- `atomcode-coding`: runtime owner and `runtime/goal_questions.rs`, internal per-input context, goal commands and control introspection.
- CLI/TUI: no second blocking popup for continued requests, human-readable historical receipt, current-question commands; headless consumers do not fabricate late answers.
- daemon: pending ordinary requests survive normal turn/input boundaries but not runtime replacement; continued cards do not imply a running turn. Optional live-wire flag carries this presentation distinction.
- WebUI: retain only continued live questions across normal idle, still clear ordinary prompts and approvals. Non-live chat streams terminate with their runtime; later handling requires session resume.

## Verification

Use virtual time for deadline/cancellation tests. Cover on-time and late replies, duplicate replies, approvals and authorization-bearing questions, non-goal behavior, question-only pause, shorter-wait races, actual native restart/answer/restart and cancellation/restart, compaction, rewind, partial admission and reused completed call ids. Exercise daemon replay, TUI presentation/command classification and WebUI idle state. Run affected crate nextest suites, the compile/feature/negative-control gate, WebUI typecheck/tests and formatting checks.

This change is not an overnight Lumen soak result. Windows interaction and active text editing across the fixed foreground deadline still need manual validation. Build an official executable only through `./scripts/build-official.sh release/v5.2.4` when requested/needed.
