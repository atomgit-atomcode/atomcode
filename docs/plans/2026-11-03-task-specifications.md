# Task specifications implementation plan

Goal: users select whole original messages as durable task specifications, review them, and explicitly pin/unpin without guessing IDs.

Baseline: release/v5.2.6 at a9bfee566, clean worktree. The native event log remains authoritative. A neutral context-retention event references the existing UserMessage by seq, UTF-8 byte length and SHA-256. No second source-text store or compaction state machine. Rewind takes back both retention decisions and their sources; resume replays them.

1. Kernel: add versioned retention facts, ordered retained-source validation and projection that bypasses folding/rewrites for selected original text, without duplicating messages. Add multi-compaction, unpin, rewind and corruption tests.
2. Harness: a spec command service validates explicit actions, bounds retained bytes, commits only while idle, and reports previews/full text. Final provider dispatch validates integrity and actual message presence after request hooks. All source bytes count toward the existing request budget. Pressure may compact other history but may not silently truncate retained specifications.
3. Coding/host: session- and generation-addressed owner control for reading/updating specifications using the mounted agent and current context capacity. No model tool may pin messages. Route TUI and daemon through the same owner.
4. TUI: /spec, list, pin and unpin open a paged user-message picker; selecting a row opens original text with an explicit pin/unpin action. Localized UI, status and byte estimate. Stable IDs are internal command arguments, not required user knowledge.
5. WebUI: message pin actions beside Copy open full-text review; pinned messages can be unpinned directly. The attachment menu manages the source list, including missing-source recovery. No WebUI slash command is added. Operations are bound-session scoped and reject stale replies and active-turn mutations.
6. Verify the shared guarantees, scoped commands and frontend flows; run affected nextest suites, compile gate, rustfmt, WebUI type/build/tests, lock and diff hygiene. No commit/push unless requested.

Failure semantics: invalid/missing/corrupt retained source refuses provider dispatch. Pin over a conservative context budget is rejected before committing; later pressure must fail clearly if ordinary compaction cannot fit the request. Existing latest user instructions and approvals retain precedence. Unpin is possible to recover from damaged references. No claim that byte retention proves model obedience.

## Implemented usage and verification

TUI `/spec` or `/spec list` opens a source picker. Select a preview to review the complete message; Enter explicitly pins/unpins the reviewed source. `/spec pin` and `/spec unpin` filter the picker. `/spec show <seq>` and `/spec pin|unpin <seq>` remain expert shortcuts. WebUI uses a pin action beside each user message’s Copy icon; first pinning opens full-text review, while unpinning is direct. The attachment menu opens the complete management list. It requires the bound Sync session. Pinning never sends the source as a new user prompt.

Pinned entries precede ordinary history on every page. Missing entries stay available for unpin/recovery. Maximum eight messages; combined source text allowance is min(64 KiB, current model context tokens / 4), measured conservatively in UTF-8 bytes, displayed before pinning. Source integrity is checked before every final provider dispatch, including repeated identical source texts (each requires a distinct retained message).

Validation covers ordinary context behavior, exact UTF-8/CRLF/code fidelity across three projections, real model and fallback compaction, duplicate-source multiplicity, original display framing, busy/session boundaries, actual event-file writer failure and runtime fail-close, rewind/resume, TUI review-before-action and desktop/mobile WebUI layout and interactions. Existing full suites passed: core group 2,294 tests and driver group 3,502 tests; focused integrity and failure cases also passed. WebUI's 382 tests, typecheck, build, browser flows and compile gate passed. Final checks are rerun when source changes affect these paths. No retired-engine golden was regenerated.

A real failed-write test exposed lock reentry in idle cancellation. Agent::cancel now implements its documented idle no-op before taking the status-move lock; this also protects other between-turn edits using the same commit seam.
