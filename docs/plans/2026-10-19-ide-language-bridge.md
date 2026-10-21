# IDE language bridge implementation plan

**Goal:** First-party, explicitly bound, read-only IDE semantic queries for hover, definitions, references, diagnostics and document symbols, plus one combined inspection request.

**Architecture:** The VS Code workspace extension owns a private, on-demand loopback query server and its editor documents. An explicitly configured connection-file path selects one window; credentials remain in the private file and never enter row configuration, descriptions or model output. Capabilities owns transport and tool serialization; coding only maps configuration and mounts the existing lsp tool. No second agent/daemon owner or session format changes.

**Baseline:** release/v5.2.4 f89201f33. Existing unrelated dirty files: CLI webui_relay.rs and daemon live_api.rs. Work in /tmp/atomcode-ide-language-bridge-f89201f33; integrate only this feature after verification. No commit/push unless requested.

**Tech stack:** TypeScript, VS Code public provider commands, Node HTTP, Rust reqwest, existing LSP tool and config seams.

1. Add a versioned read-only bridge contract, bounded authenticated loopback server, explicit start/copy-config and stop commands. Per-window private connection file; no agent or daemon launch required for queries.
2. Implement editor query adapter and position resolver: one-based UTF-16 columns or literal symbol plus occurrence; ambiguity is an error. Include document version, dirty flag, source and unknown capability status. Discard results if the document changes while querying. Diagnostics are labeled editor snapshots, not build verification.
3. Extend existing lsp tool with an explicit IDE binding/backend, document_symbols and inspect; keep local LSP opt-in and no silent semantic-to-text fallback. Validate the file, endpoint, workspace, limits, deadline and cancellation. Never print connection contents or credential-bearing errors.
4. Test actual HTTP auth/scoping/lifecycle with a fake editor host; test Unicode positions, stale documents, output bounds, unavailable providers and timeouts. Rust tests validate connection security, workspace selection, schema/result contracts, cancellation and tool registration.
5. Add user setup and wire documentation. Run extension typecheck/build/tests, affected Rust suites, fmt/diff/lock checks and compile gate. Real VS Code/CodeArts/Cangjie and Windows/WSL manual validation remains explicit if unavailable locally.

Out of scope: automatic multi-window discovery, write operations, code actions, formatting/rename, full Remote/WSL productization, changing extension daemon selection, implicit bridge startup or downloading language servers.

## Verification

- Config: 317 library tests passed. Coding: 539 passed, 8 pre-existing ignored. Capabilities with lsp: 1122 passed, including real loopback auth, workspace scoping, no credential echo, redirects, response bounds, complete semantic result validation and in-flight cancellation.
- Extension: TypeScript no-emit check, webview build and all 20 existing/new test files passed. The bridge test exercises the actual HTTP server with a fake editor host, including dirty-buffer metadata, Unicode/ambiguous positions, stale-document rejection, batched inspection, timeout classification and bounded outstanding providers after disconnect.
- Standalone capabilities (`--no-default-features --features lsp --lib`) passed after making the existing tool dependency explicit. No new packages or lock changes.
- Full compile gate passed in the isolated feature checkout, including feature and negative-control checks. Fmt/diff checks passed.
- No runnable VS Code installation was found here. Actual VS Code/CodeArts/Cangjie, Windows, WSL/remote and real language-server latency/token savings have not been verified. This is a read-only pilot, not completion of the later multi-window/write-operation scope.

Integrated into the main release worktree without committing. The main extension compile/build and full compile gate passed after the final response-contract validation. Both pre-existing dirty Rust files were preserved byte-for-byte; concurrent daemon/preview work was left intact. Cargo.lock is unchanged.
