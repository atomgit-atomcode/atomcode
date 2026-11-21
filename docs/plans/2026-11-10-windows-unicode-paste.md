# Windows Unicode paste preservation

Baseline: release/v5.2.7 e68ef5e91. Preserve unrelated headless_json.rs and main.rs edits.

Owner/boundary: normalize committed text at the vendored Windows console-record parser, where VK_MENU + key_up + Unicode is distinguishable from a genuine character key release. Keep the TUI release filter and buffer sanitization unchanged. No runtime/session/provider lifecycle changes.

Implementation:
1. Emit one Press for the BMP Alt-release character commit; retain surrogate-pair merging and ordinary release semantics.
2. Verify console code pages after attempting UTF-8 setup, before taking interactive input. Fail visibly if input or output setup failed. Update the pre-initialization warning so it does not imply the value is measured during paste.
3. Cover native Windows records, the report's exact text and space/newline fidelity through the TUI buffer, plus setup failure states. Add the Windows-only parser tests to the existing Windows job using the vendor manifest (dependency-package selection does not resolve vendor test dev-dependencies).
4. Run focused/full TUI tests, Windows cross-check, workspace compile gate and format checks. Record Windows runtime limitations explicitly; do not claim the report machine reproduced locally.

Verification: focused input/setup tests 4 passed; full atomcode-tui 1952 passed, 0 skipped; workspace compile gate passed; workspace fmt check and vendored parser rustfmt check passed. Windows-native parser production/test code cross-checked with `cargo check --manifest-path vendor/crossterm/Cargo.toml --lib --tests --target x86_64-pc-windows-msvc`. Dependency-package test selection from the root does not include its dev-dependencies; the vendor manifest is required. No Windows binary ran locally; actual VS Code clipboard/console event reproduction remains unverified and CI execution is pending. Cargo.lock forbidden package audit passed. Existing headless edits are preserved; only the pre-TUI warning in main.rs belongs to this fix.
