# Recoverable memory TTL implementation plan

**Goal:** Optional expiry, visible expired/upcoming entries, and exact renewal without deleting facts.

**Baseline:** release/v5.2.4 at 85ee71bf98a5bfc82603ffb17912332603e113a9, existing memory phases and unrelated dirty files preserved.

**Architecture:** Capabilities remains the sole Markdown store owner. `expires_at` is optional RFC3339 metadata; absent means no expiry, including all legacy entries. `ttl_days` is a positive integer resolved once on an explicit write, never reset by unrelated changes. Expiry is derived at a captured UTC instant and never writes on read. New session selection excludes expired entries, including pinned entries; existing session facts remain unchanged. No timer, lifecycle hook, deletion or second persistence owner.

**Commands:** `/remember --ttl-days N TEXT`; `/memory update ID --ttl-days N`; `--clear-expiry`; `/memory --expired`; `/memory --expiring-within N`. Tool actions expose the same fields. Query filters are list-only, mutually exclusive and compose with topic/status. Upcoming means active, not yet expired, within N days. Invalid timestamps/durations/options fail before managed writes.

**Files:** memory/entry.rs (metadata/clock-explicit predicates), context.rs (selection/report), store.rs (dedup and load), input.rs and tools/memory.rs (shared syntax/actions), harness capabilities.rs (filtered source DTO), i18n and docs/memory.md.

**Verification:** Deterministic fixed-time boundary tests, old records, pinned expiry, renewal/clear and no implicit renewal, invalid inputs preserve bytes, shared command parsing, harness list filters/current-log preservation, affected crates, fmt and compile gate. No commit/push.

## Result

Implemented optional `expires_at`, bounded whole-day durations, exact renewal/removal, expired/upcoming filters in the shared parser, tool and harness, and localized expired diagnostics. Eligible selection and legacy adapter reads exclude expired entries; large-file pin scanning excludes expired pins before retaining its bounded candidates. Stored metadata and old session logs remain untouched on read. Upcoming windows and writes use checked date arithmetic.

Validation: capabilities full lib passed 1,107 cases before final overflow/scan refinements; final targeted memory/parser suite passed 72. Harness lib passed 74 and capabilities integration passed 27, including expiry/renewal and unchanged history. TUI lib passed 1,574 serial cases. All four compile-gate checks passed; fmt/diff and lock audits complete. The previously documented independent memory-only feature-combination failure remains outside scope. No commit or push.

## Pre-commit audit

Audited on release/v5.2.4 at 85ee71bf98a5bfc82603ffb17912332603e113a9. Found and fixed a stateless daemon driver gap: its old parser could store new flags as content. Daemon memory requests now use the shared grammar and tool; original keyword deletion retains its existing removed-string response through the same atomic store. Existing response kinds remain for original requests, and new memory actions carry a dedicated text result consumed by the web UI. Replaced environment-dependent old daemon memory tests with a hermetic fixture covering local scope, TTL, filtered listing, exact deprecation/restoration, rejected duration and legacy response compatibility. No runtime/session state is rebuilt or modified.

Final verification: selected capabilities/harness/TUI full suites passed; daemon lib passed 347; web UI typecheck and 290 tests passed. Build, final compile gate, fmt and staged-file audit are recorded at commit delivery. The optional memory-only baseline issue documented above remains unrelated. User authorized commit and push after audit; unrelated files stay unstaged.
