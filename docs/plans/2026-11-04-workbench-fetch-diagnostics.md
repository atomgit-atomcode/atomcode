# Workbench fetch diagnostics implementation plan

Baseline: release/v5.2.6 at e6af903ce071f744500b5e1d73c3504c1654ee8b, with unrelated model-card changes preserved.

The daemon owns API routing; browser transport owns relay credentials; Workbench owns ephemeral directory loading state. No runtime or session persistence changes. Missing relay credentials fail closed without a request. Unknown API paths fail with JSON 404 before SPA/dev redirects. HTML success responses become explicit API errors. Network errors retain their original evidence; this does not claim a confirmed Windows root cause.

1. Reject unknown /api and /workbench endpoints at the static fallback, preserving ordinary SPA paths.
2. Return a structured relay_unpaired diagnostic without contacting the server; detect HTML in shared JSON parsing.
3. Render loading, failed/retry and empty states separately; ignore stale directory responses.
4. Add regression tests for routing, pairing, and HTML responses; run WebUI tests/typecheck/build, daemon tests, formatting and compile gate. Windows real-device reproduction remains unavailable.

Validation: WebUI 391 tests passed; typecheck and production build passed. Daemon 388 library tests passed. Browser fixture verified loading state, HTML success diagnostic, retry to an empty directory, and unpaired relay failure without local endpoint fallback. Screenshot: /tmp/atomcode-workbench-diagnostics.png. Rust formatting and diff whitespace checks passed. Cargo.lock unchanged and contains none of the prohibited packages. Workspace compile gate passed, including feature-gated checks and both negative controls.

Pre-submit audit (baseline d4287e31d): reviewed shared JSON parsing, fail-closed relay pairing, daemon fallback boundaries, stale directory responses, file/image toolbar behavior, tab/last-file restoration and nested fullscreen Escape. WebUI 391 tests and daemon 388 tests passed; both browser regression scripts passed; typecheck/build, formatting and whitespace checks passed. No Windows real-device reproduction; this change improves diagnostics rather than claiming the reported network root cause is confirmed. Concurrent cd-recent cleanup changes are excluded.
