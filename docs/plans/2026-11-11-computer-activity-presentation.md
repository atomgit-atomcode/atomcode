> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer activity presentation

Baseline: eeafc349f, clean isolated worktree. Reference: local Codex history_cell/mcp.rs and thread_transcript/computer_groups.rs.

The TUI remains a projection of existing session events. Classify only exact managed MCP server aliases for display, never authorization. Reuse existing folded tool runs and expansion, separate browser, computer and ordinary runs, and stop at turn boundaries. Keep interrupted calls visible individually. Existing visible blocks break runs; tool-result image rendering is absent in the current transcript projection and remains a separate delivery gap; hidden reasoning remains transparent. Add localized settled group labels and preserve failure counts. No runtime, protocol, persistence or dependency change.

Verify mixed runs, turn boundaries, expansion membership, earlier failures, interruption and existing TUI rendering tests. Run affected crate suites, format and compile gate before delivery. Native GUI acceptance still requires OS permissions for the exact experimental App.

## Verification

TUI + i18n: 2037 passed; additional interruption regression: 1 passed. `gates/compile.sh`, format check and diff check passed. Cargo.lock unchanged, H index flag, forbidden private crypto package names all zero. No dependency or bundled executable added. Native GUI acceptance remains pending OS grants; tool-result screenshots still need a dedicated projection.
