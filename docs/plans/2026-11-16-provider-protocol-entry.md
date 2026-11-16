# Provider protocol entry implementation plan

**Goal:** Offer OpenAI / Anthropic directly when connecting a TaoToken or Alibaba account, with visible, matching endpoint addresses in TUI and WebUI.

**Architecture:** The config preset catalog owns supported connection choices and paired URLs. Drivers render that data and submit an existing preset/protocol plus an explicit endpoint through ProviderBook. No new persisted account schema or runtime owner. Unknown custom URLs remain unchanged; known billing routes map only to their matching protocol route.

**Tech Stack:** Rust config, CLI, TUI and daemon; Preact/TypeScript WebUI.

1. Add curated protocol choices and endpoint pairs to the config catalog; test scheme, host and plan preservation.
2. Supply the catalog to TUI and daemon DTOs. Expose protocol and endpoint in the basic TUI connection form; keep vendor account naming while saving the selected existing protocol.
3. Add an optional protocol selection to account creation. Validate it against the chosen vendor, resolve default endpoints server-side, and reject unsupported choices before writes.
4. Add the WebUI selector and visible address. Match discovery and save requests to the chosen protocol, reset incompatible staged model selections on protocol changes, preserve keys and unrelated form data.
5. Test TUI transitions and save output; test backend validation and catalog serialization; test WebUI switching, billing routes, custom addresses and desktop/mobile interaction. Run affected tests, formatting and compile gate. Preserve unrelated dirty changes; use an isolated worktree for final compile validation if the Qwen work still blocks the main worktree.

Baseline: branch release/v5.2.8, HEAD 906d2022927a17179520fe6c6c3eb2d979bb31eb. Worktree includes previous WebUI/Anthropic discovery fixes and unrelated Qwen changes. This task does not migrate existing accounts or change their billing routes.

## Validation and release audit

- WebUI typecheck, unit tests and production build passed. Desktop/mobile browser
  regressions for protocol entry, model/effort controls and grouped history passed.
  `npm run test:provider-protocol` now exposes the new regression directly.
- Full affected-crate run (config, capabilities, TUI, daemon and CLI): 6,140 tests,
  6,139 passed and 21 skipped; the sole failure exposed existing uninstall manifest
  drift. Both fallback scripts were corrected separately in `5f8e872b5`; the shell parity
  regression passed on rerun. PowerShell is not installed on this
  machine, so its runtime parity test returns early; its credential list was also
  checked statically against the Rust manifest.
- The account API fixture now verifies that comments remain present rather than
  requiring a trailing document comment to remain at the beginning after tables
  are inserted. The existing preservation checks before those insertions remain.
- The additive account protocol field uses the existing account storage writer;
  gateway protections, credential ownership and custom endpoint behavior remain.
  The final merge is validated by `gates/compile.sh` before pushing.
- Historical Qwen cache experiments and browser temporary artifacts are excluded.
