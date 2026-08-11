# TUI Config Panel Implementation Plan
> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a searchable half-screen `/config` panel that edits non-provider settings safely and applies supported changes without restarting AtomCode.

**Architecture:** `atomcode-config` owns a UI-neutral setting catalog and comment-preserving TOML patch transactions. TUI owns modal interaction and rendering. Runtime-affecting commits continue through the existing config revision/reconcile and `CodingRuntime` lifecycle; the panel never creates a second runtime owner.

**Tech Stack:** Rust, `toml_edit`, crossterm, existing `ConfigStore`, modal renderer, and `CodingRuntime`.

---

### Task 1: Comment-preserving config transactions

**Files:**
- Modify: `crates/atomcode-config/Cargo.toml`
- Modify: `crates/atomcode-config/src/store.rs`

1. Add failing tests proving a scalar patch preserves comments, unknown keys, and unrelated provider text.
2. Add a locked document-patch API with optional revision matching.
3. Parse and validate the patched document before atomic replacement.
4. Run `cargo test -p atomcode-config store`.

### Task 2: UI-neutral setting catalog

**Files:**
- Create: `crates/atomcode-config/src/settings.rs`
- Modify: `crates/atomcode-config/src/lib.rs`

1. Add catalog tests for exclusion of model/provider/credential fields.
2. Define stable setting IDs, TOML paths, value types, defaults, search aliases, and apply policy.
3. Add typed read/parse/patch/reset helpers for supported scalar settings.
4. Run `cargo test -p atomcode-config settings`.

### Task 3: Searchable half-screen `/config` modal

**Files:**
- Create: `crates/atomcode-tuix/src/modals/config_panel.rs`
- Modify: `crates/atomcode-tuix/src/modals/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`
- Modify: `crates/atomcode-tuix/src/render/*` only where existing menu chrome needs a config-specific variant.

1. Add state tests for search, navigation, boolean toggle, enum cycling, edit validation, and reset confirmation.
2. Render settings with current/default/modified markers in `/resume`-style half-screen chrome.
3. Replace the old `/config` help output with modal installation.
4. Run the focused TUI modal tests.

### Task 4: Save, apply, rollback, and concurrent edits

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/config_panel.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`

1. Commit each edit through `ConfigStore` against the latest document.
2. Reuse existing persisted-config reconciliation for UI/next-turn/runtime-reprepare changes.
3. On apply failure, rollback only when the committed revision is still current; otherwise reconcile newer disk state.
4. Add tests for concurrent unrelated edits and failed-apply rollback.

### Task 5: Localization, docs, and verification

**Files:**
- Modify: TUI/config i18n resources as needed
- Modify: `site/docs/en/keybindings.html`
- Modify: `site/docs/zh/keybindings.html`
- Modify: configuration reference docs

1. Add localized labels, help, validation, and apply-status messages.
2. Document `/config`, excluded provider/model fields, reset semantics, and restart-required markers.
3. Run affected crate tests, inspect `git diff --check`, and report any unverified interactive terminal behavior.
