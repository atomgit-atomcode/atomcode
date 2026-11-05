# Long session WebUI implementation plan

> Implementation proceeds in this session under the user's approval. No deployment or version changes.

**Goal:** Keep mobile and desktop timelines usable through overnight sessions without removing transcript content or changing runtime ownership.

**Architecture:** Split long assistant displays into small chronological segments. Keep a bounded recent payload cache and a smaller mounted window; archive cold immutable payloads to IndexedDB only after a successful transaction. Keep lightweight navigation metadata in memory. Page and search archived data on demand, and construct full-turn clipboard text only on explicit copy. Preserve pending tools/steers, stream generations, approvals, and persisted history.

**Tech Stack:** Preact, TypeScript, IndexedDB, Node tests, Playwright browser checks.

## Tasks

1. Add pure timeline segment/window/metadata helpers and regression tests, including a 1,000-tool turn, pending parallel tools, identical timestamps, and lossless reconstruction.
2. Add a tab-owned archive with transaction-before-eviction, bounded read cache, expiry cleanup, and failure-safe fallback. Add a timeline hook for bounded payload retention and asynchronous reads/search/copy. Do not discard data if browser storage fails.
3. Integrate mobile/desktop paging, navigation and search into Chat. Replace eager whole-turn copy strings with a lazy resolver. Bound caches for inactive sessions. Keep backend transcript/history access and current turn state intact.
4. Ensure SSE readers release/cancel their bodies and watchdog reconnects respect backoff. Retain the current generation fence and do not automatically disconnect healthy background execution.
5. Run focused and full frontend tests, typecheck and build. Exercise long histories and paging in a local browser; verify archive failures and session switches. Inspect diffs and integrate only these changes into the original atomcode checkout, preserving unrelated edits.

## Design decisions

A DOM-only window leaves large output strings resident, while dropping old messages depends on reconstructing transient UI events from the backend. A browser archive preserves those display events and permits eviction after a committed write. The backend remains the durable session authority; the archive is an expiring display cache. If storage is unavailable, keep original payloads and still bound mounted DOM. Never claim a hard memory guarantee under storage failure or for one arbitrarily large active event.

## Validation limits

A synthetic long-session test can prove retention/render bounds and historical accessibility, but cannot prove the original iPhone WebKit crash is fixed without device validation. The relay queue overflow policy is a separate server issue; this change does not alter its wire protocol or deployment.

## Implemented and verified

- Mobile: 12 mounted segments, 36 recent payload segments, approximate 1 MiB page / 2 MiB recent-payload budgets. Desktop: 32 / 96 segments and 3 MiB / 8 MiB budgets. Each immutable assistant segment has at most eight parts. One oversized entry and unresolved parallel calls remain readable/mutable rather than being discarded to satisfy a budget.
- IndexedDB transactions are limited by both entry count and payload weight. Cold metadata replaces payloads only after commit; object identity rejects stale archive completions. Reads used by search/copy are pinned against cleanup. Serial fences protect newly committed writes before their metadata renders. Unreferenced snapshot payloads and abandoned document owners are reclaimed independently of other tabs.
- Inactive session caches retain at most three recent tails; full history reloads from the daemon. Cache freshness compares normalized segment counts, preserving an uncommitted recent tail when persisted history is stale.
- Live pages preserve a scroll anchor, load adjacent pages on gestures, and keep an old reading page stable as events arrive. Persisted history loads one/two turns per batch and mounts at most three/six turns; each long archived turn also uses a segment window.
- Whole-turn clipboard text is assembled only on demand. Search scans cold payloads without retaining the whole transcript and is not repeatedly cancelled by live tokens. Lightweight artifact metadata preserves full-turn links.
- Live SSE readers cancel/release on completion and failure; the watchdog respects pending reconnect backoff and old stream activity cannot update the new generation's heartbeat.

Validation: `npm test` (370 passing), `npm run typecheck`, `npm run build`, `npm run test:timeline`, `cargo fmt --all -- --check`, and `git diff --check`. Browser regressions use synthetic data and a mocked clipboard: mobile/desktop history paging and search, complete ~1.08M-character copy, live updates while reading older messages, a single 1,000-tool assistant, denied storage, transaction abort atomicity, cache owner isolation, read pins, and serial-fenced cleanup. Browser screenshots were inspected. Build retains the existing large-chunk advisory.

The original iPhone overnight failure is not reproduced by this synthetic test. No release bundle, version, relay deployment, daemon lifecycle, or persisted session format is changed. Initial `/live` snapshots still arrive as a single event; these optimizations bound retained display detail after hydration rather than asserting a hard process-memory ceiling for arbitrarily large individual events or unavailable browser storage.

## Pre-push review follow-up

The combined relay/WebUI review found and fixed byte-budget paging gaps near the end of a timeline, direct navigation that could miss a heavy target, and missing gesture paging inside a large persisted turn. Nested turn edges now coordinate with outer history paging; the existing styles are reused. Live event dispatch exceptions propagate to the reconnect path after cancelling/unlocking the failed reader, and hydrating one new cold entry preserves already-loaded page entries.

Final frontend validation: 373 tests pass, typecheck/build pass, browser regressions additionally cover automatic paging inside a persisted 400-step turn. The workspace/feature compile gate (including both negative canaries) passes. No runtime/session format, version, or official relay asset catalog was changed.
