# Common model card data snapshot

Scope: prepare one offline JSON file and a repeatable exporter from the local whatmodel checkout. No model-picker UI, network fetch, provider configuration changes or automatic model routing in this step. Current AtomCode model configuration remains authoritative for context, vision and tool capabilities.

Select the twelve explicit model identities reviewed against the user's usage tables. Preserve checkpoint identity; unresolved gateway, preview and floating aliases remain explicitly unmapped. Export valid published evaluation records with their benchmark definitions, exact source ledger entries and source notices. Use whatmodel's complete dataset and original strengths/scoring modules before selecting model cards, preserving each comparison-set identity. Do not calculate ranks from the selected twelve.

Source links and attribution remain in the snapshot for a future data-information view, without requiring source badges or links on the main card. Retain the upstream data notice and data licence separately from AtomCode code licensing. Save content hashes and upstream commit so data can be traced even if the local checkout is dirty.

Verify exact selection, evaluation references, source review status, score preservation, exclusions of invalid runs, comparison scopes, no local paths/secrets, deterministic export and failure without overwriting the output. Do not change the unrelated search-activity work already dirty in the worktree (baseline c37314193).

Result: data/model-cards.json contains 12 model cards, 300 valid evaluation records, 130 benchmark definitions, 39 reviewed sources and 6 original LLM task definitions. Upstream commit 2769f5ba536d5d78fc70a772048512471a499802. Exact upstream preservation and deterministic/full-population subset export integration tests passed (2/2); standalone checked-in snapshot validation passed with the optional upstream test skipped. JavaScript syntax and git diff checks passed. No runtime integration, commit or push performed.

## TUI and WebUI integration

Approved follow-up: both model selectors get a read-only model card viewer and two-card comparison. The capabilities crate owns an embedded, neutral read-only catalogue and conservative identifier matching; it introduces no config or runtime dependency. Daemon exposes the same catalogue on an authenticated read endpoint and verified card IDs/configured context/vision on model rows. WebUI loads data only on opening details. TUI uses its existing provider panel state and key/render seam, without a config dependency or any runtime commands for viewing cards.

Unknown/floating/private aliases show an explicit missing mapping while allowing manual catalogue browsing. Viewing or comparison must never select a runtime model. Raw evaluation rows retain conditions; task scores compare only within the same comparison set. Source/attribution links stay in a separate data-notes view. Existing configuration is authoritative for context and vision. Handle no data, sparse results, keyboard back/scroll/compare, English/Chinese and narrow screens.


Integration validation: the four affected Rust crate suites passed (4631 tests, 1 skipped); final shared binding/unit/daemon projection regressions passed (8 tests). WebUI typecheck, 389 unit tests, production build and Playwright desktop/mobile tests passed, including English/dark and Chinese/light, lazy loading, two-card comparison, missing mapping, sparse data, retry, Escape, source disclosure and no model-switch writes. The compile gate and rustfmt/diff/lock checks passed. Upstream export fidelity/determinism integration tests passed after moving explicit wire bindings into the data file.

Extra static diagnostics: the global TUI i18n gate already fails at committed HEAD with the same 23 hardcoded-CJK occurrences; no new occurrence was introduced. Existing TUI layers/string-slice diagnostics also remain outside this change (no new indexed string slices or decorative-glyph literals in this patch). Their baselines and unrelated code were not modified. Implementation checks recorded before submission.


Follow-up: distinguish explicitly reviewed absence of public evaluation records (`lumen-alpha`) from an unverified alias. Store that state in the snapshot; neither invent zero scores nor disable model use. A known no-data card opens without fetching the catalogue and offers manual reference-card browsing; both TUI and WebUI retain the original identity/no-data message while browsing. Future upstream appearance requires explicit review in the exporter.


No-public-data verification: 9 Rust card/selector/API regressions passed; WebUI typecheck/build, 389 unit tests and desktop/mobile browser tests passed. Browser coverage explicitly opens `lumen-alpha` twice with zero catalogue fetches or score rows, then manually browses a reference card without changing the configured model. Export fidelity tests passed with the no-data registry included. Added a standalone test that a formerly absent model appearing upstream fails before replacing a valid snapshot.


Pre-submit review: completed at release/v5.2.6 e6af903ce. Fixed Shift+Tab reverse comparison traversal and added exporter-hash verification to catch stale snapshots after binding-rule edits. Fresh verification passed: 4636 Rust tests (1 skipped), 391 WebUI unit tests, 3 live export/snapshot tests, WebUI typecheck/build and desktop/mobile browser regression. Default and feature-gated checks passed; the compile gate feature-canary self-test required one isolated rerun, which passed in full. Existing unrelated static-gate diagnostics remain as documented above. Parallel workbench-fetch changes are excluded from this submission, including the unrelated api.ts readJson hunk.
