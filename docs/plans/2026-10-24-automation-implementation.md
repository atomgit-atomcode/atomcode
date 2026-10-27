# Automation Implementation Plan

**Goal:** Complete the accepted shared automation management, TUI/WebUI and
natural-language tools, with self-tests. Original-session reminders remain the
explicit later phase from the accepted proposal.

**Architecture:** A focused schedule host service owns definitions, registration
and run metadata. Existing native headless execution remains the only job
runtime. All frontends/tools use the same service; no browser or second host
timer is added.

**Tech Stack:** Rust, fs2 process leases, atomic JSON definitions, append-only
run events, existing OS adapters, Axum, Preact/TypeScript.

Baseline: release/v5.2.5 c97c78656; preserve unrelated dirty work. Reference
`2026-10-24-automation-schedule-review.md` for audited source and boundaries.

1. Add `atomcode-schedule` shared store/service, atomic revisions and independent
   append-only run records. Test conflict, deletion while running, corruption,
   lease exclusion, restart history and failure recovery in isolated temp dirs.
2. Extract OS adapters from CLI, explicit executable selection. Validate rules,
   report registration failures separately, make time displays honest and
   weekday/timezone/anchor correct. Test injected platform command runners.
3. Route CLI add/edit/remove/enable/disable/run/history through shared service;
   executor acquires lease, records all outcomes and session binding, never
   rewrites definition at completion. Preserve unattended policy behavior.
4. Add authenticated daemon service operations and correlated run launch with
   idempotency; protect project paths and reject conflicts. API tests cover
   auth, errors, mutation replay and persisted state.
5. Upgrade TUI /schedule to keyboard-operable management with create/edit,
   enable/disable, run, history and session navigation via established seams.
6. Add WebUI automation navigation/list/detail/forms and run/session links;
   mobile layout and reconnect reads; preserve unrelated turn-navigation work.
7. Register lead-only schedule model tools through host-injected service and
   explicit approval; tools and direct UI share schemas and validation.
8. Self-test all requirements, inspect rendered mobile/desktop UI, run relevant
   nextest suites, frontend tests/typecheck/build, fmt and compile gates. Update
   docs with actual behavior and platform verification limits. No auto publish.

## Progress (2026-10-25)

Implemented steps 1–7: shared service and extracted OS adapters, revision-safe
CLI execution, authenticated daemon API, TUI keyboard management, WebUI mobile
and desktop management, lead-only model tools with mandatory explicit consent.
Added reverse-paged run history, durable registration errors and manual request
receipts, project scoping, crash recovery, host-local/DST preview tests, and
explicit product-home propagation to OS jobs. See ../automation.md and ADR 0028.

Validation: shared suite 39/39, CLI 583/583, coding 1174/1174 (9 configured
skips), capabilities 1137/1137, i18n 73/73, frontend 319/319. Frontend typecheck,
production build, mocked-browser desktop/mobile interaction and compile gate
passed. Automation API auth/project/revision/replay/history tests passed.
Daemon full suite: 361/362 passed; chat_offers_the_mcp_tools_on_its_first_turn
still fails in isolation with automation injection removed. This independent MCP
readiness failure is retained and reported, not hidden by weakening assertions.
The i18n suite also exposed two pre-existing literal duplicates; both tables now
forward to the product entries with the same text.

Platform adapters use injected command runners; no actual OS task registration
or production model/relay request is performed. No commit/publish.

## Pre-push audit (2026-10-25)

An isolated checkout of only the selected changes passed the locked workspace
check, full compile gate, fmt, schedule 39/39 and frontend 319/319 with typecheck
and production build. Audit found that internally approved automation could pass
the generic Plan risk check; the shared Plan verdict now blocks create/update/
delete while retaining read-only list. Its regression, Auto consent, mounting
and existing Plan criteria passed 12/12. The isolated daemon suite reproduces
the same independent MCP first-turn failure (361/362); its automation API passes.
Unrelated worktree edits and private crypto dependencies are excluded.
