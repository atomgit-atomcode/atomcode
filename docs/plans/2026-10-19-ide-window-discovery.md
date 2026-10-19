# IDE window discovery implementation plan

**Goal:** Discover live IDE windows by working directory, select a window explicitly when ambiguous, and expose connection status without querying a language provider.

**Architecture:** The workspace extension owns each read-only bridge and private connection file. Enabling publishes it under the host's user data directory; an opt-in `lsp.ide_discovery_dir` passes that directory as data to capabilities. Explicit `ide_connection_file` wins. The existing lsp tool gains `status` and `window_id`; authenticated, bounded health probes filter dead entries, unique matches connect, ambiguous matches fail closed. No session/runtime ownership or persistence changes; no automatic native-LSP fallback.

**Baseline:** release/v5.2.4 18ee021c4. Preserve unrelated dirty CLI webui_relay.rs, daemon live_api.rs and preview files. Implement in /tmp/atomcode-ide-discovery-18ee021c4; integrate only own patch. No commit/push in this implementation turn.

**Tech Stack:** Existing Rust config/coding/capabilities seams, reqwest/futures, Node HTTP, VS Code workspace extension.

1. Add optional discovery directory config, mount and describe it through existing owner; old explicit bindings remain valid. Add status/window selection to lsp without requiring a source file for status.
2. Implement bounded private-file scan and authenticated status probes; canonical workspace matching, cancellation/deadline, stale/invalid entry reporting, no credential or raw error disclosure. Never select across multiple live matches or silently replace a failed explicit binding.
3. Extension publishes to shared host directory, copies stable discovery config, provides current-window binding command and status-bar/command feedback. Clean up on disable/workspace changes; HTTP status does not invoke editor providers or consume stalled query slots.
4. Test live/dead/multiple/wrong-workspace/explicit windows, invalid and private files, cancellation, old bridge compatibility, credentials, Unicode roots and lifecycle. Typecheck/build/test extension and affected Rust crates; run fmt, lock/diff and compile gate.
5. Update setup/troubleshooting and manual Windows/Cangjie/WSL acceptance checklist. Real IDE testing is reported as unverified if no executable/environment is available.

## Verification and delivery

- Config: 318 library tests passed. Coding: 539 passed, 8 existing ignored. Capabilities with lsp: 1130 passed, including window discovery, authenticated health, ambiguity/explicit selection, stale entries, cancellation, private directory/file validation and selected-window replacement rejection.
- Extension: TypeScript compile/webview build and all 22 test files passed. Actual registration with a mocked VS Code API exercises trust, shared-directory publication, concurrent enable, status UI commands, explicit binding, workspace changes and cleanup. HTTP tests verify health without provider calls, including when all query slots remain occupied.
- nextest is not installed here; used per-crate cargo test. No VS Code executable or Windows/Cangjie/WSL runtime is available; manual acceptance remains unverified and is documented in docs/ide-language-bridge.md.
- Discovery remains opt-in. Users copy a stable directory once; existing explicit bindings still win and are never silently replaced. Status health requires the new extension, while older explicit bridges can still serve queries.
- No runtime lifecycle/session storage, daemon ownership, write tools, version or lock changes. Full compile gate and integration checks are recorded below when complete. No commit/push requested in this implementation turn.

Isolated checkout: compile gate passed (workspace/all targets, feature gates and both negative controls); fmt and diff checks passed. Final codeintel regression additionally checks portable Windows path serialization and cancellation-test bounds.

Main worktree integration: all 19 feature files matched the isolated checkout; original dirty CLI/daemon files remained byte-for-byte unchanged. Main extension compile/build and all 22 tests passed, as did the final full compile gate, fmt/diff and product-directory gate (baseline zero). Working and index Cargo.lock contain zero private dependency markers and are unchanged. Release audit found no blocking issues. The authorized release commit includes only these 19 feature files; unrelated work remains outside the commit.
