# Windows QR Rendering Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Ensure onboarding either renders a standards-polarity, scannable QR code or falls back to an actionable URL without displaying a broken pseudo-QR.

**Architecture:** Separate reliable compact QR rendering from generic UTF-8 encoding support. Modern emulators receive an explicitly colored half-block renderer; legacy Windows consoles receive a font-independent ANSI background-space renderer only when the complete QR fits, otherwise onboarding shows the URL. All rendering paths validate width and height before returning rows.

**Tech Stack:** Rust, crossterm retained-cell SGR parsing, qrcode 0.14, atomcode-tuix unit/integration tests.

---

### Task 1: Pin terminal capability semantics

**Files:**
- Modify: `crates/atomcode-tuix/src/terminal.rs`

1. Remove the code-page probe so UTF-8 encoding cannot classify a console as a modern emulator.
2. Keep forced Unicode independent from the legacy-conhost classification.
3. Run the terminal capability tests.

### Task 2: Add reliable bounded QR renderers

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/qr.rs`

1. Add tests for standard black-on-white polarity, four-module quiet zone, SGR reset, forbidden block glyphs in legacy fallback, and width/height rejection.
2. Replace theme-dependent `Dense1x2` output with explicit black/white foreground and background colors.
3. Replace the legacy `██` fallback with background-colored spaces.
4. Return `None` whenever the selected representation cannot fit completely.
5. Run QR renderer tests.

### Task 3: Wire terminal bounds into onboarding

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/onboarding_wizard.rs`

1. Pass terminal rows, the QR content budget, and a QR-specific half-block reliability signal to the renderer.
2. Show the explicit browser fallback copy whenever no reliable QR representation fits.
3. Update onboarding tests for compact QR, legacy renderer, 80×24 URL fallback, and forced-Unicode legacy conhost.
4. Run onboarding tests.

### Task 4: Verify and audit

**Files:**
- Verify only the files above; preserve all pre-existing dirty files.

1. Run `cargo test -p atomcode-tuix`.
2. Run `git diff --check`.
3. Inspect the final diff for SGR leakage, bounds errors, theme dependence, and accidental overlap with user changes.
