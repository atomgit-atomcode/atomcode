# Project capability setup implementation plan

**Goal:** Make `/setup` report missing project capabilities and offer confirmed ast-grep installation and optional Context7 configuration.

**Architecture:** Setup owns the pure recommendation catalog; the CLI local service owns detection, installation and recommendation notice state. The TUI uses its existing setup port and list sheets. MCP configuration remains owned by the MCP config module. No agent lifecycle changes, new kernel commands, automatic trust or credentials in row config.

**Tech stack:** Rust, existing setup scanner, MCP config parser, `which`, Tokio child processes, existing TUI sheets and CLI Clap options.

## Baseline and boundaries

- Branch `release/v5.2.7`, commit `aa61a3ef79d2347e8d38a72a4f8c5b26cc4767f0`; clean worktree.
- Checked setup run/state/scanner, CLI setup and TUI seed port, MCP config/trust partition, LSP registration and recent history.
- Existing `/setup` installs the setup skill on demand then invokes it; preserve that behavior under an explicit automation-recommendation option.
- Setup is a driver/local service. Reads never start LSP or MCP servers and report configuration/binary presence without claiming connection health.
- Context7 is added disabled, without credentials or approval grants, preserving existing config and refusing collisions/corrupt input. Enabling remains an explicit `/mcp` action even in a trusted project.
- Installation is a user-confirmed local command, with failure/timeout reported and binary identity verified after success. Inspection/startup never installs or connects.
- Notice state records recommendation identities/revisions separately from seed state, so seed reinstalls and dependency manifest edits do not cause repeated notices. Unknown future state must not be overwritten.

## Task 1: Recommendation data and safe MCP edit

- Add `crates/atomcode-capabilities/src/setup/recommendations.rs`: small static catalog for ast-grep, Context7, and per-stack native LSP suggestions.
- Extend `crates/atomcode-capabilities/src/mcp/config.rs` with an add-only disabled HTTP entry text editor; use existing parser and map handling, refuse overwrites and malformed input.
- Test Rust/Python/mixed/empty catalogs, preservation, legacy alias handling, collisions and disabled/trust semantics.

## Task 2: Host capability service

- Add `crates/atomcode-cli/src/project_setup.rs`; export from `lib.rs`.
- Read existing effective LSP/MCP configs; use server identity rather than just display name; validate ast-grep version instead of accepting any `sg` executable.
- Generate and execute a bounded, confirmed install plan using supported package managers; recheck plan identity before running and verify installed binary afterward.
- Add versioned per-project notice state; do not use changing manifest hashes for reminder frequency.
- Tests use temporary directories and fixture executables; never install actual dependencies or mutate the developer's configuration.

## Task 3: TUI and CLI entry points

- Extend `crates/atomcode-tui/src/setup.rs` with neutral capability view/plan types and local-service methods.
- Update `tui_setup.rs` and `commands.rs`: capability list, explicit confirmation sheet, skip, force recheck, and existing automation skill path.
- Update `main.rs`: `setup --check`, `--install`, `--yes`, `--skip`; existing bare CLI setup still installs seed skill and reports capabilities.
- Add one nonblocking first-project hint through existing opening notice/welcome flow; persist only when displayed.
- Test command routing: checks never invoke agent/install; unconfirmed actions only show plan; confirmation executes exact plan; automation path preserves seed/reload behavior.

## Task 4: Documentation and verification

- Document commands, installation scope, optional Context7, disabled configuration and trust distinction.
- Run focused nextest tests, then affected crate tests with relevant features; run `bash gates/compile.sh`, product-directory gate, `cargo fmt --all` and `cargo fmt --all -- --check`; review diff and Cargo.lock.
- No actual third-party installation, network MCP connection or release is part of verification.
