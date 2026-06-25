# LSP in v2 — diagnostics tool + TUI connect-status

**Date:** 2026-06-25
**Branch:** `feat/collapse-bridge`
**Status:** Design approved, pending implementation plan

## Problem

The v1 agent engine gave the model an LSP `diagnostics` tool (type errors, missing
imports, etc. without a full build) and surfaced language-server start/fail status in
the TUI. After the v1 engine was deleted, v2 (the native kernel engine) ships **no LSP
capability**: the model cannot query diagnostics, and there is no LSP status display.

### Root cause (grounded)

The LSP capability is *fully implemented and functional* in
`atomcode-capabilities/src/codeintel/lsp/` (834 LOC — `client`/`jsonrpc`/`manager`/
`registry`/`types`; `ensure_server` spawns a real language server, syncs documents,
reads diagnostics, degrades gracefully when the binary is absent). It is gated behind
the capabilities `lsp` feature. **No crate in the v2 build chain enables that feature:**

- `coding/Cargo.toml` requests capabilities features
  `["provider","tools","web","codeintel","skills","mcp","session","memory","cc-hooks"]`
  — `codeintel` is on (symbol/graph tools work), but **`lsp` is not**.
- `cli`/`daemon` declare `features = ["tools"]` only.

With `lsp` off: `DiagnosticsTool` is `#[cfg]`-compiled out, and `codeintel_tool_names()`
omits `"diagnostics"`, so it is never registered or mounted.

`DiagnosticsTool` is **self-contained on-demand**: each call reads the file fresh from
disk, calls `notify_file_changed` (which `ensure_server`s + syncs the doc), waits the
settle delay, then reads diagnostics. So enabling the feature alone makes on-demand
diagnostics work — no edit→LSP sync loop is required for correctness.

### What v1 actually provided (for scope reference)

1. **`diagnostics` tool** — on-demand query (`file_path` + `severity`). The capabilities
   version is a faithful port; only the feature flag differs.
2. **edit/write/search_replace auto-sync** via `core` `notify_lsp_file_changed` — only
   *synced* the server (result discarded); it did **not** surface diagnostics after
   edits. Redundant in v2 because the v2 diagnostics tool self-syncs from disk.
3. **TUI connect-status** (✓/✗ when a server starts/fails) via `core`'s
   `lsp_connect_rx` event channel (now dead — its feeder, the v1 `tool_context`-owned
   `LspManager`, was deleted in D2.1b, so `ToolContext.lsp` is always `None` in v2).

## Scope (decided)

**A** (diagnostics tool) **+ ③** (TUI connect-status). The v1 edit-sync (②) is excluded
(redundant in v2). The TUI status is delivered via the kernel `ProgressSink` seam.

Out of scope (follow-ons): ② edit-sync; deletion of core's now-vestigial v1 LSP
subsystem (`LspManager` + `ToolContext.lsp` + `notify_lsp_file_changed` + core
`DiagnosticsTool`) — a separate delete-dead change that touches core tool hot-paths.

## Design

Three crates change. **`core` is not touched** for the feature itself (the inverted
architecture means v2 uses the capabilities LSP); `core` only loses the dead
`build_lsp_manager*`/`LspConnectEvent` in the cleanup section.

### A — enable the `lsp` feature

- `coding/Cargo.toml`: add `"lsp"` to the capabilities feature list. `coding` is the
  assembler that calls `register_codeintel_tools` + mounts `codeintel_tool_names()`, so
  it is the natural owner.
- Effect: `DiagnosticsTool` compiles, is registered by `register_codeintel_tools`, and
  `codeintel_tool_names()` includes `"diagnostics"` → it is mounted into the live
  registry that the coding agent assembles (both `assemble.rs` and `parts.rs` paths).
- Cargo feature unification: `cli`/`daemon` depend on `coding`, so their binaries get
  capabilities-with-`lsp` via the union. **Verification:** build `cli`/`daemon` and
  assert the `diagnostics` tool is present; if a resolver quirk prevents unification,
  add `"lsp"` explicitly to `cli`/`daemon` too.
- New transitive deps (`which`, `url`, `tokio` process/io-util/fs) are already in the
  workspace tree — negligible impact.
- **Laziness preserved:** a server is spawned only when the model *calls* `diagnostics`.
  Headless/CI with no language server installed degrades gracefully (no eager spawn).

### ③ — connect-status via `ProgressSink`

The `LspManager` is created inside `register_codeintel_tools` (L1, tool layer) and owned
by `DiagnosticsTool` — the driver never constructs it. So the v2-correct seam for
status events is the kernel `ProgressSink` (`kernel/tool.rs:86`), the designated
tool→driver channel: `ctx.progress.emit(msg)` becomes `AgentEvent::ToolProgress
{ call_id, message }`, which tuix **already renders** (`native/translate.rs:100`).

- `capabilities` `LspManager`: enrich `notify_file_changed`'s return from `bool` to a
  structured outcome so the tool can report status. Proposed shape:
  - `Synced { server: String, newly_started: bool }`
  - `Unsupported { ext: String }`
  - `Failed { server: String, error: String }`
  The manager stays a **pure library** (no `ProgressSink` dependency); UX-string
  formatting lives in the tool. `notify_file_changed`'s only caller is `DiagnosticsTool`,
  so this is a single-caller signature change.
- `DiagnosticsTool::execute`: map the outcome to a `ctx.progress.emit(...)` line, then
  continue the existing diagnostics flow (the `Unsupported`/`Failed` cases also drive the
  existing graceful-degrade output text):
  - `Synced{server, newly_started:true}` → `"LSP: started {server} for .{ext} ✓"`
  - `Synced{newly_started:false}` → emit nothing (server already running — avoid
    repeating the status line on every subsequent `diagnostics` call)
  - `Unsupported{ext}` → `"LSP: no language server for .{ext} (not installed / not configured)"`
  - `Failed{server, error}` → `"LSP: {server} failed to start: {error}"`
- No new event type or channel. Status renders inline with the `diagnostics` call's
  progress, which is the only `ensure_server` trigger in this scope.

### Cleanup — remove the superseded dead `lsp_connect_rx` plumbing

Once `ProgressSink` carries status, the old `core` connect-event path is both fully dead
and a duplicate display path. Remove it as part of this change so only one LSP-display
path exists:

- `cli/main.rs`: `build_lsp_manager`/`build_lsp_manager_with_events` call,
  `_lsp_manager_keepalive`, and the `lsp_connect_rx` argument to `tuix::run`.
- `tuix`: the `lsp_connect_rx` parameter on `run()`, the `EventLoopContext.lsp_connect_rx`
  field, and the two `select!` arms (`event_loop/mod.rs:3595`, `:3988`).
- `core`: `build_lsp_manager`, `build_lsp_manager_with_events`, `LspConnectEvent`, and
  the `LspManager` `connect_events`/`with_events`/`emit`-to-channel machinery (the
  `LspManager::new` no-events constructor and the rest of the core LSP module stay — they
  are part of the larger, separate v1-LSP-removal follow-on).

This is compiler-verified: after removal, `cargo check --workspace` with 0 errors proves
no live consumer remained.

## Testing

- **A:** assert (in the coding assemble test path) that `diagnostics` is in the mounted
  tool set when the `lsp` feature is on; build `cli`/`daemon` to confirm the feature
  reaches the binaries.
- **③:** unit-test the outcome→progress-string mapping with a mock `ProgressSink` that
  captures `emit`s, covering `Synced` / `Unsupported` / `Failed`. The no-server-installed
  path is deterministic (reuse the existing `missing_binary_manager` / empty-registry
  pattern). The real-server path stays gated behind `lsp-e2e`.
- **Regression:** after the `lsp_connect_rx` removal, `cargo check --workspace` clean +
  all lib test suites green (core/tuix/daemon/cli/shell).

## Risks & mitigations

- **Feature unification not reaching cli/daemon:** verified by build; fallback is an
  explicit `"lsp"` on those crates.
- **Spawning a real language server in CI:** lazy + graceful degrade means no server is
  started unless the model calls `diagnostics` and a binary is present; no eager spawn.
- **`notify_file_changed` return change:** single caller (`DiagnosticsTool`), so the
  blast radius is contained.
- **Removing the tuix `select!` arms (zero-e2e area):** the removed arms consumed an
  always-empty channel, so removal is behavior-neutral; compiler + existing 997 tuix lib
  tests + a manual TUI run cover it.
