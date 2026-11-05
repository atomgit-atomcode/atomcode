# Windows baseline follow-up Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Preserve the Windows baseline improvements without bypassing live proxies or losing gitignore rules for relative/symlink roots.

**Architecture:** FileIndex detects VCS ownership from the resolved root but retains the original FileIndex root and relative entry representation. Provider route policy bypasses only explicit ConnectionRefused; a Windows socket remains available after its bounded connect attempt so pending Winsock errors can be read instead of treating timeout as refusal. No runtime, provider lifecycle or session ownership changes.

**Tech Stack:** Rust, ignore, socket2 (already locked), nextest.

---

Baseline: release/v5.2.6 @ 6d5de950405d41fe0f442a313cdb059198cba197, clean worktree.

1. Add regressions for the reviewed relative root and Unix symlink root, and
   restore timeout-as-unknown expectations. Confirm the failures on merged code.
2. Resolve the internal walk path before scanning ancestors; leave the caller's
   FileIndex root and returned relative paths unchanged. Keep .git/.jj/worktree checks and non-repository ignore behavior.
3. Use existing socket2 as a Windows/provider-only direct dependency. Use bounded Winsock select with both writable and exception sets, querying
   SO_ERROR on failure or timeout: preserve timeout when absent,
   return the actual refusal when present. Retain existing Unix probe behavior.
4. Test pending-error classification, live/closed listeners and proxy routing.
   Keep unknown, overloaded and timed-out proxies; bypass explicit refusals only.
5. Run capabilities nextest, Windows-target checks where toolchain support allows,
   workspace compile and format gates. Review Cargo.lock for only the direct edge;
   no new package versions or private crypto dependencies may enter it.

Physical Windows testing remains needed to confirm the native stopped-port case.

## Validation

- All three reviewed regressions failed against the merged code, then passed.
- 60 focused index/proxy tests and 1217 capabilities tests passed.
- Workspace/feature compile gate passed on a non-overlapping rerun; rustfmt and
  diff checks passed. Cargo.lock adds only the existing socket2 dependency edge.
- The exact Windows probe functions compile for x86_64-pc-windows-msvc in a small
  check crate with the same locked socket2/windows-sys versions.
- A complete Windows provider cross-check was attempted but blocked in ring's
  native C build by the host's missing Windows C toolchain; it is not reported as
  a passing Windows build. Native Windows runtime verification remains pending.

## Follow-up performance work

The user authorized continuing the adjacent improvements. A debug-build,
metadata-only measurement on this checkout took 153/79/76ms for three successive
uncached queries. This is evidence of repeated walk cost, not a release benchmark.

- Deduplicate socket-address probes only within a refresh; preserve unknown errors.
- A pending FileIndex walk owns a cooperative cancellation flag. Reset/drop stops
  the old traversal, including allowlist passes. Prune .git before descending.
- Add a complete-first-query cached search on FileIndex, reusing its existing
  3-second background refresh. The daemon holds at most eight canonical search
  roots, partitioned by product directory identity, with per-root locks so walks
  do not run under the cache-map lock. Eviction drops/cancels unused indexes.
- Verify multiple query prefixes share full entries, roots/identities are isolated,
  refresh discovers new files, eviction rebuilds, and reset/drop cancels work.

Performance follow-up validation: capabilities 1220/1220 and daemon 398/398
passed. A same-process debug comparison found uncached walks at 86/73/76ms,
initial complete cache construction at 165ms, then cached filters at 0.60/0.36ms,
with identical result counts. This does not measure release or HTTP/UI latency.
Cache freshness is demand-driven stale-while-refresh, not a strict three-second
visibility deadline. Drop/reset cancellation is cooperative; an OS filesystem
operation already underway cannot be interrupted.
