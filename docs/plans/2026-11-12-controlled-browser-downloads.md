# Controlled browser downloads implementation plan

**Goal:** Add a backend-owned HTTP(S) download publication boundary before enabling per-origin download grants.

**Architecture:** The external browser backend owns transfer jobs and artifacts; CodingRuntime remains the sole agent owner. Chromium implicit downloads stay disabled. Every redirect must pass explicit access and download permission checks before a request, and only complete bounded responses reach generated artifact paths. Failed or cancelled transfers never retry automatically.

**Tech stack:** Node built-ins, node:test, existing optional MCP backend.

Baseline: release/v5.2.7 at 7d6eb260fcc0ec9382a965200a5ba3d1241bc802, clean worktree. Local Codex 806d9732c9 config/browser_use.rs separates access/downloads/uploads/full_cdp_access. ADR 0032 rejects cancellation-only download enforcement.

1. Add `integrations/browser-origin-scope/transfers.test.cjs`: positive artifact hash, permission denied before dispatch, redirect permission refusal, size limits, truncated responses, cancellation and cleanup, no replay, generated filenames.
2. Implement `transfers.cjs`: explicit policy callback, manual redirects, bounded identity-encoded streaming, private staging, exclusive atomic publication, transfer cancellation and permanent shutdown. Initial transport is unauthenticated HTTP(S); no caller headers, credentials, paths or browser cookie forwarding. Unsupported content encoding is rejected. Errors omit URL/query details.
3. Review tests and implementation together. Run existing v1 policy regressions. Keep the module unexposed until MCP cancellation, scoped policy v2, and browser context authentication receive integration tests.
4. Integrate through `policy.cjs`/`backend.cjs`, add managed CLI configuration and actual-browser tests in subsequent verified units. Upload/CDP, blob downloads, real UI and other-platform acceptance remain open in the full delivery audit.

Failure semantics: cancellation before synchronous publication removes staging and publishes nothing. Once the exclusive link succeeds, the artifact is committed; lost responses are uncertain outcomes and must not cause replay. Close aborts and awaits owned jobs. No browser event cancellation is treated as authorization.

## First implementation result

The internal publisher and 14 transfer tests are implemented. Tests use actual loopback HTTP streams and private temporary files, including chunked overflow, incomplete bodies, cancelled/stalled requests, denied redirect destinations with zero requests, request-ID collisions and late cancellation after a committed artifact. Existing 13 policy tests continue to pass. No new tool or policy version is advertised, and no existing browser behavior changes. Authenticated context downloads, compression, blob URLs, MCP cancellation integration and policy-v2 configuration remain subsequent work; compressed responses currently fail closed.

Validation: `node --test integrations/browser-origin-scope/transfers.test.cjs integrations/browser-origin-scope/policy.test.cjs` passes 27/27; both new scripts pass `node --check`; `cargo fmt --all` and its check pass; `CARGO_TARGET_DIR=/tmp/atomcode-cu-guidance-target bash gates/compile.sh` passes workspace, feature and negative-control gates. Cargo.lock remains unchanged, normal H index flag, all three forbidden package counts zero. A first test run exposed a FileHandle stream completion hang; the writer now explicitly awaits writes and owns descriptor closure, and the corrected tests exit cleanly.
