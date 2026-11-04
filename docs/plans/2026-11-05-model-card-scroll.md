# TUI model card summary and stable scrolling

Baseline 19a8d116a6dedba9d3477e6c21b0e42e7e36d992. Preserve unrelated Windows/proxy/dependency changes.

Root cause: layout height counts only remaining lines after scroll; bounds permit scrolling to the final individual line even when all content fits. Compute visible row count from total content and available panel height, independent of offset. Bound display and keyboard/wheel offsets to the final full page using the same viewport calculation. Host supplies actual rendered panel geometry; measurement does not mutate state. No runtime/model selection changes.

Regression: measure/draw agreement across widths and offsets, short cards cannot scroll when fully visible, constrained terminals reach the final page and can immediately scroll back. Run TUI library tests and formatting; compile gate before delivery.

Approved scope update: TUI becomes a single-model summary table (task, score, rank/total). Remove comparison state and Tab/BackTab actions; retain explicit configured versus viewed identity, strong/weak task summaries and data notes. Full benchmark evidence/comparison remains in WebUI, with a /webui and model info-button hint. Existing /model and /provider share the same F2 entry; no new runtime or command route. Add localized viewing/read-only/table labels and regression that Tab does not enter comparison.

Validation: 1773 TUI/i18n library tests passed; full i18n suite 73 tests passed. Full compile gate passed including feature variants and negative controls; rustfmt check and diff whitespace check passed. Full TUI crate suite passed: 1903 tests including e2e. WebUI and runtime/model switching behavior are unchanged.

Pre-submit audit: verified shared /model and /provider F2 entry, keyboard and wheel viewport bounds, offset-independent measure/draw heights, configured versus browsed identity, terminal-cell table alignment, localized help with no comparison action, and WebUI guidance. No blocking findings. Existing full test/compile results above apply to the unchanged implementation; rustfmt and whitespace checks passed again. Exclude concurrent Windows/proxy/mention-search/Cargo changes from the commit.

Publication check: concurrent private crypto overlay polluted the shared working lockfile during audit. Publish from a clean detached checkout containing only these seven files; its lockfile has zero hkdf/hmac/zeroize_derive entries, and its full compile gate (including negative controls) passed. Preserve the shared overlay and all unrelated working changes.
