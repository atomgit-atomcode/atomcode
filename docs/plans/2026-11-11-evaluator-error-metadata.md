# Evaluator error metadata

Baseline: release/v5.2.7 at d4d336e59, clean worktree. Reviewed evaluator producers (open and stream failure), runtime consumers (recovery, goal progress and ControllerWarning), ProviderError fields and recent controllers history.

The coding runtime continues to own goal recovery, generation and terminal results. Replace debug-formatted provider errors in evaluator result messages with fixed text and optional numeric HTTP status. Keep transient classification on original structured fields and preserve retry-after. Record only numeric/bool metadata in diagnostics; never include upstream message/code. No protocol, session format or lifecycle changes.

Extend the existing production-reader privacy regression to open/stream failures with 401, 429 and 503. First reproduce leakage, then run auxiliary tests, coding tests, compile gate and format/lock checks. Preserve public crypto stub and lock; build signed executable only through an isolated private overlay.

## Results

The baseline regression failed with the simulated private message/code exposed in GoalResult. Corrected the fixture to explicitly mark retryable 429/503 errors per the existing provider contract; no product retry-policy change was made. Final auxiliary tests: 8 passed. Full coding tests: 1337 passed, 11 skipped. Compile gate, fmt check and diff check passed. Tests cover both request-open and stream errors, safe result/log contents, HTTP status, Retry-After, generation/controller/epoch, one request and no evidence expansion.

Signed isolated release build passed. New executable completed all 11 loopback-SSE main/title/session nonregressions, including resume and unchanged body text. Goal-error coverage is from direct production-reader tests; no live gateway or real API credentials were used. No upstream model-format root cause is claimed fixed.

Release SHA-256: `8bf6ed9075a2db4439624869318486e5053d3bc89d755858da0a5fcef0e90f8e`. Evidence: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-evaluator-error-evidence-7w5m6d1b`. Root Cargo.lock and public crypto stub were kept intact; temporary private worktree/overlay removed after archiving.
