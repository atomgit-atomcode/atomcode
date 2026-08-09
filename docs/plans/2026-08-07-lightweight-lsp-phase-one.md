# Lightweight LSP Phase One Implementation Plan

This plan records the implementation boundary and verification sequence for the first
read-only LSP phase.

**Goal:** Add an opt-in, read-only `lsp` model tool with definition, references, hover, and diagnostics operations that starts only locally installed language servers on first use and degrades without failing the turn.

**Architecture:** `atomcode-capabilities` owns the neutral LSP protocol client, workspace-scoped manager, and tool. `atomcode-coding` maps `[lsp]` configuration into a neutral settings DTO and injects one manager-backed tool into the runtime-owned tool registry. Kernel and drivers remain unaware of LSP processes; dropping the runtime-owned tool graph terminates spawned children.

**Tech Stack:** Rust, Tokio stdio processes, LSP JSON-RPC, serde/serde_json, existing AtomCode kernel tool API.

---

### Task 1: Define protocol queries with deterministic tests

**Files:**
- Modify: `crates/atomcode-capabilities/src/codeintel/lsp/client.rs`
- Modify: `crates/atomcode-capabilities/src/codeintel/lsp/types.rs`

**Steps:**
1. Extend the injected mock LSP transport to answer hover, definition, and references requests.
2. Add failing tests asserting zero-based wire positions and normalized results.
3. Add read-only client query methods with bounded request timeouts.
4. Run `cargo test -p atomcode-capabilities --features lsp codeintel::lsp::client`.

### Task 2: Make the manager workspace-safe and failure-tolerant

**Files:**
- Modify: `crates/atomcode-capabilities/src/codeintel/lsp/manager.rs`
- Modify: `crates/atomcode-capabilities/src/codeintel/lsp/registry.rs`

**Steps:**
1. Add tests proving clients/failures are keyed by normalized workspace root plus language rather than extension alone.
2. Add a startup timeout and cache unavailable/broken server outcomes to prevent retry loops.
3. Expose manager methods for document sync and the three semantic queries.
4. Preserve graceful degradation when a server is unconfigured, missing, fails startup, or times out.
5. Run the manager test module.

### Task 3: Add the unified read-only `lsp` tool

**Files:**
- Create: `crates/atomcode-capabilities/src/codeintel/lsp_tool.rs`
- Modify: `crates/atomcode-capabilities/src/codeintel/mod.rs`
- Modify: `crates/atomcode-capabilities/src/codeintel/diagnostics.rs`

**Steps:**
1. Add failing schema/validation/degradation tests for `definition`, `references`, `hover`, and `diagnostics`.
2. Implement one `lsp` tool requiring `file_path`; semantic operations also require one-based `line` and `character`.
3. Normalize locations to project-relative `file:line:column`, bound output size, and retain diagnostic severity filtering.
4. Retain `DiagnosticsTool` only as a compatibility facade for existing embedders; do not auto-register or mount it in new coding runtimes.
5. Keep ordinary Tree-sitter/text code-intelligence registration independent from LSP registration.
6. Run all codeintel tests with `--features lsp`.

### Task 4: Wire configuration into the runtime owner

**Files:**
- Modify: `crates/atomcode-capabilities/Cargo.toml`
- Modify: `crates/atomcode-coding/Cargo.toml`
- Modify: `crates/atomcode-coding/src/config.rs`
- Modify: `crates/atomcode-coding/src/assemble.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`

**Steps:**
1. Add tests mapping `Config.lsp` into a neutral LSP settings DTO.
2. Enable the capabilities `lsp` build feature only through the coding assembly.
3. Register and mount `lsp` only when `enabled=true`; merge built-ins only when `auto_detect=true`, with explicit servers overriding them.
4. Ensure provider reload/session rebuild paths use the same `CodingAgentConfig` policy.
5. Run coding configuration and assembly tests.

### Task 5: Verify and audit

**Files:**
- Review all files above; do not include unrelated dirty-worktree files.

**Steps:**
1. Run `cargo fmt --all -- --check`, formatting only this change if required.
2. Run `cargo test -p atomcode-capabilities --features lsp codeintel`.
3. Run `cargo test -p atomcode-coding`.
4. Run targeted daemon/CLI configuration tests if the shared runtime config changed their compilation surface.
5. Inspect `git diff` for lifecycle ownership, missing terminal states, accidental default-on behavior, and unrelated changes.
