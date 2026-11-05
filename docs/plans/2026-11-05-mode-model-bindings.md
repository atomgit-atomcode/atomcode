# Mode model bindings implementation plan

Goal: optional Plan/Build model and reasoning bindings, configured once and applied on cross-mode transitions across TUI and WebUI.

Baseline: ca887e19a5d5bc1c13965db7b7fc3fa2a28fce76 on release/v5.2.6. Implement in isolated feat/mode-model-bindings checkout to preserve concurrent schedule, recovery, follow-up and runtime edits.

Architecture: CodingRuntime remains the only mode/provider lifecycle owner. Resolve configured selection IDs through the existing model registry and provider factory. Prepare and patch the existing harness provider seam before committing mode flags and presentation events; failures retain the previous state. No separate agent owner, kernel command or credential-bearing row config. Active-turn model changes fail explicitly, while unconfigured mode changes retain existing behavior. Automatic changes never write default_model. Track pre-Plan selection/effort in runtime-owned state scoped to session/cwd; explicit Build binding wins, otherwise restore it. Same-mode selection is a no-op. Manual model changes are temporary relative to the next cross-mode transition.

1. Config: add optional [modes.plan]/[modes.build] model selection and reasoning_effort; shared validation, sanitized choices and document patches; round-trip and invalid-model/effort tests.
2. Runtime: reuse resolved-provider config and harness swap for atomic idle transitions; emit provider/effort/mode updates after success. Cover entry/restore/build override/same mode/unsupported effort/provider failure/busy/session retention.
3. TUI /config: model choice and supported effort fields from shared sanitized catalog; reload configuration through the existing settings apply path. Feedback for temporary /model use and bound-mode switches.
4. Daemon/WebUI: sanitized mode binding API and settings panel, mode-menu configuration entry, explicit pending/failure/result feedback. Share runtime behavior; do not implement front-end /model sequences.
5. Documentation: config example, defaults, partial configuration, manual override and running-task behavior. Verification: affected crate tests, WebUI typecheck/tests/build, browser regression, cargo fmt/check and full compile gate. Integrate only owned changes into the shared worktree after validation; commit/push only after the user-requested audit.

## Implementation notes

- Live state remains owned by CodingRuntime. The existing idle provider seam is patched without replacing the agent or changing its generation. Host presentation caches refresh from an owner query after provider changes.
- Saving WebUI preferences uses a metadata-only runtime command; a TUI preferences-only reload similarly updates the registry without resetting a temporary selection or interrupting a task.
- Independent WebUI requests carry their explicit model and effort each turn. The shared configuration resolver validates their next selection; a live agent is always changed through CodingRuntime.
- An effort-only Build binding restores the pre-Plan model first, then overrides effort. The saved selection is scoped to session and working directory, and cleared after session replacement.
- Startup and same-mode selections do not reapply a binding. Preferences apply on the next actual cross-mode transition. Automatic transitions never persist a global default.

## Verification

- WebUI: 395 unit tests, TypeScript check, production build, and browser regressions at 1200px/390px, including repeated chat requests retaining bound effort, unsupported effort reset, temporary overrides, restoration and rejection feedback.
- Full compilation gate passed in the isolated feature checkout. The integration uses three-way merges against ca887e19a and preserves unrelated working-tree edits.
- Rust: 3412 affected-crate library tests passed (8 skipped) in the isolated checkout; integrated targeted regressions passed 64/64.
- Integrated full compilation gate, cargo fmt --all -- --check and git diff --check passed. Cargo.lock is unchanged and contains none of hkdf, hmac or zeroize_derive.
- The subsequent user-requested audit authorizes committing the reviewed mode bindings and session-browser UX changes to release/v5.2.6; unrelated recovery and skills edits remain outside the commit.
- Audit fix: mode selectors use declared canonical effort levels instead of inventing levels for fixed-effort profiles; inherited and restored profile defaults remain valid. Config and runtime regressions cover this boundary.

Final release audit: 3423 affected-crate library tests passed (8 skipped), 400 frontend tests passed, TypeScript/build and desktop/mobile browser suites passed; full compile gate, formatting and diff checks passed on the exact 32-file release scope in a clean checkout.
