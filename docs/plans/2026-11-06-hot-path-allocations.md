# Hot-path allocation reduction implementation plan

**Goal:** remove verified redundant allocations without changing rendered bytes, request bodies, snapshots or permission decisions.
**Architecture:** local changes in existing layers, no new dependencies, protocol, runtime owner, schema cache or global scratch buffer. Immutable kernel tool definitions remain tied to the same mounted snapshot. Harness ToolsSvc definitions are intentionally fresh/dynamic; its request-hook copies are not an existing Arc deep-copy and remain unchanged. Raster snapshots retain their Arc/COW ownership.
**Tech stack:** Rust standard library and existing workspace tests.

Baseline: a86dfaf30 on release/v5.2.7. Work in isolated feat/hot-path-allocs; preserve concurrent recap changes. Each numbered change is an independent commit.

1. TUI whitespace flattening: write words directly into one string, reuse in the prompt header; differential Unicode/empty-input tests and TUI tests.
2. Anthropic system-message joining: no temporary Vec; preserve separators between empty system messages and exact request-body behavior; provider tests.
3. Kernel tool snapshot: add shared defs_arc getter, use in engine; keep owned defs API; pointer-sharing and snapshot-publication regression, kernel tests. No caching of dynamic harness definitions.
4. SGR: encode directly into the existing output buffer with fmt::Write, preserving style order, capability mapping and reset/OSC behavior. Differential reference encoder plus TUI renderer/emulator tests.
5. Raster: nested string-keyed maps for borrowed lookups, retain immutable old views, lengths, duplicate mount/refusal and module isolation; raster and TUI tests.
6. Bash grants: direct normalized whitespace assembly and invocation reconstruction without temporary vectors. Preserve parsing, comment stripping, quoting and deny precedence; differential and permission tests.

Do not touch cold-path joins, reasoning-loop vectors, release profiles or schema clone/caching. Do not claim measured latency improvements without a benchmark. Final validation: affected crate suites, cargo fmt --all/--check, diff check, Cargo.lock inspection, and full gates/compile.sh. Integrate separate commits into the current branch without overwriting unrelated changes; no push requested.
