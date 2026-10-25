# Automation Interaction Alignment Implementation Plan

**Goal:** Align DSH management behavior while retaining independent OS-triggered sessions and AtomCode's existing WebUI structure.

**Architecture:** Keep the shared schedule service and native execution owner. Reuse AtomCode's modal, card, form, confirmation and composer seams. DSH is behavioral evidence, not a layout/template source.

**Tech Stack:** Preact/TypeScript, existing WebUI theme/components, Rust schedule service, nextest.

Baseline: AtomCode release/v5.2.5 6a0916a64; DSH 687f282e86. Work in an isolated checkout; preserve unrelated edits. The user explicitly selected management alignment while retaining independent sessions.

## Compared behavior

| Area | DSH | AtomCode alignment |
| --- | --- | --- |
| Create | New conversation with prefilled scheduling request | Primary conversation creation, secondary manual form; never auto-submit |
| Browse | Search, status filtering, next-run order | Same concepts; AtomCode modal and expandable task cards |
| Edit | Local draft, save/cancel, preserved on refresh | Three-way refresh merge for untouched fields; overlapping edits require explicit resolution |
| Timing edits | Equivalent rule does not reset timer; changed interval starts from save | Shared service avoids registration on content-only/no-op changes; reset interval anchor only when timing changes or resumes |
| Feedback | Loading/failure/empty/conflict kept separate | Retain catalog/history on refresh failure; no fake empty result |
| History | Newest saved facts, paged, no invented prompts | Keep grouped actual execution results and native run-session links |
| Delete | Confirm, authoritative refresh, visible failure | Reuse AtomCode ConfirmDialog; retain row until refreshed catalog confirms deletion |
| After deletion | Reminder and delivery records removed, original session retained | Keep independent execution records and sessions; deleting a schedule never deletes conversation history |
| Close/navigation | Late responses cannot replace current task | Abort reads and scope results to current editor lifetime |
| Execution | Original-session inbox reminder | Intentionally unchanged: independent native sessions, pause/run controls retained |
| Timing vocabulary | Once, cron, arbitrary zone and intervals | Only offer actually supported host-local OS rules; do not imply unsupported capabilities |

## Steps

1. Add pure form/rebase/catalog helpers and tests for refresh merges, conflicts, validation and ordering.
2. Fix shared save semantics with injected scheduler tests: content changes/no-op do not re-register; timing changes reset interval origin; stale revisions still fail.
3. Rebuild Automation using AtomCode modal/card/confirmation/form primitives; add cancel, dirty navigation, status/order/next-time summaries and safe paginated history.
4. Add a scoped composer-prefill handoff for conversation creation; new-session failure stays visible and no request auto-sends.
5. Run frontend tests/typecheck/build and isolated browser flows in light/dark and mobile; run schedule tests, appropriate integration tests, fmt and compile gate. Transfer only owned files to the shared worktree. No automatic commit/push.

## Implemented and validation

Management semantics are aligned within the confirmed scope. The WebUI uses
AtomCode's modal/account-card structure, shared confirmations, controls and theme
tokens. New conversation creation is scoped to its opener, preserves unsent text
and never auto-submits. Manual configuration remains available. Polling rebases
untouched fields; overlapping edits require explicit resolution. Confirmation
revisions are frozen, and catalog/history failures retain previous content.

Shared save skips equivalent changes and content-only OS registration. Rule edits
and interval resume update the preview anchor. The systemd interval adapter also
has an initial OnActiveSec and restarts only the timer on rule registration; OS
last-activation/catch-up/precision remain platform semantics, not a DSH host timer
guarantee. Source: https://www.freedesktop.org/software/systemd/man/257/systemd.timer.html

Verification: 325 frontend tests, typecheck/build, 42 schedule tests and
6 actual CLI executor tests passed. Browser smoke tests cover light/dark/mobile,
creation without auto-send, cancel, concurrent refresh, conflict resolution,
failed reads/history retention, deletion retry and native session navigation.
The authenticated automation API integration, final compile gate, fmt, frontend typecheck/build and post-fix browser reruns all passed. The shared worktree receives the exact verified files; unrelated changes are preserved. No production scheduler
registration or relay is used by these tests. No automatic commit or push.

## Pre-push audit

Audit fixed partial new-draft protection: changing only timing or permissions now
counts as an unsaved draft even before its name/instructions are filled. The
manager compares every editable field to the original new form, rather than only
checking text. Browser regression covers both cases alongside create/edit/rebase,
frozen delete revision, failure retry, retained history, session navigation and
chat prefill without submission. The isolated selected changes pass 325 frontend
tests, 42 schedule tests, typecheck/build, compile gate and fmt. Unrelated changes
are excluded; actual OS scheduling still requires platform validation.
