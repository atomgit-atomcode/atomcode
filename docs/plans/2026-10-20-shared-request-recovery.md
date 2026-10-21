# Shared request recovery budget implementation plan

**Goal:** Bound recovery across provider OPEN, ordinary retry, rate limiting, overflow and partial-stream continuation without a total response deadline.

**Architecture:** A runtime-only neutral counter travels in ChatOptions; the existing native harness initializes it once per logical model round. The provider boundary counts each attempt; adapters delegate transient retries to the harness, while their bounded authentication replay also consumes the same counter. Existing recovery handlers reserve waits from a shared cumulative allowance. No new lifecycle owner or durable session event is introduced.

**Tech Stack:** Rust, native harness waterfall, Tokio virtual-time tests, HTTP mock provider tests.

Baseline: release/v5.2.4 at 2dc0bc7cc. Preserve earlier ordinary-retry edits and concurrent tool-generation observation changes in events/agent_loop/handle/TUI.

Policy: default six total attempts and 300 seconds cumulative recovery backoff/rate-limit waiting per model round. This is not a wall-clock response deadline: OPEN/first-content/idle retain their existing watchdogs; progressing output is never timed out by this counter. Explicit upstream retry count wins, otherwise an explicit provider total-attempt count becomes the shared count, otherwise six. Stream continuation keeps its stricter per-turn safe replay cap. Goal cooldown remains owned by its existing controller.

1. Add neutral shared count/wait mechanism and serialization tests in kernel provider options; keep it excluded from serde and config dumps.
2. Initialize once in llm-retry, count at native provider boundary, preserve across retry/429/overflow/image reduction/continuation. Emit structured attempt/reason/wait totals and preserve original failure classification when refusing a recovery.
3. Disable adapter transient retry/reopen for budgeted calls; direct adapter callers retain old policy. Count any bounded auth replay against the same budget; keep connection pool repair available for the next outer attempt.
4. Charge ordinary retry, rate-limit and safe continuation sleeps to the same allowance; check before expensive overflow recovery. Preserve cancellation, partial text and uncertain-tool guards.
5. Add virtual-time mixed-failure, wait allowance, cancellation, fresh-round reset, model switch, healthy long stream and HTTP adapter request-count tests.
6. Run focused tests, affected crate suites, formatting and compile gate sequentially. Do not build official binary or commit/push unless requested.

## Validation and delivery

- Kernel/capabilities/harness/coding regression: final run 3664 passed; two unchanged 75-second runtime cases passed in the preceding run (3666 selected tests covered in total). Existing nextest exclusions remain unchanged.
- The initial source guard failure came from a test PluginRegistry named `catalog`, lexically mistaken for a ToolsSvc binding. Renamed it `recovery_plugins`; the source guard passes without changing the guard.
- HTTP integration verifies all four adapters surface budgeted 503 immediately; standalone behavior remains covered. OAuth 401 replay consumes an additional shared attempt and cannot bypass a one-attempt cap.
- Native mixed 503/429/partial-stream tests retain one allowance and preserve each partial once. Coding-specific 429 waits also share the allowance. A new model round receives a fresh allowance; explicit provider totals and upstream overrides are verified through runtime provider switches.
- A 780-second progressing response succeeds after spending its entire recovery wait allowance (virtual clock), preserving the existing content watchdog behavior.
- `cargo fmt --all -- --check`, `git diff --check`, and all four phases of `bash gates/compile.sh` passed.
- No release binary build, live Lumen soak, commit or push performed. Concurrent MCP/tool-generation changes were preserved.
