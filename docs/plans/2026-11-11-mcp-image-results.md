# MCP image results implementation plan

**Goal:** Make inline MCP screenshots reach the model through the existing coding runtime.

**Architecture:** The MCP client and registry retain the structured `CallToolResult`; the tool adapter converts it into the existing kernel `ToolResult`. The runtime, harness event log, request projection, image budget and provider adapters remain the owners of their existing responsibilities. No new lifecycle, event vocabulary, persistence format or native computer-call protocol is introduced.

**Tech stack:** Rust, serde, image, base64, Tokio, existing stdio MCP fixture and coding runtime criteria.

## Baseline and scope

- Branch `release/v5.2.7`, commit `1d946853286c98d2e7ec05b3b19b22ef18edb5bb`; worktree clean before this task.
- Current production caller of `McpRegistry::call_tool` is `McpToolAdapter`; remaining direct callers are tests. The registry currently flattens text and discards images. The adapter always returns an empty image list.
- Support inline `image` blocks and text. Do not fetch external resource URLs or add desktop actions. Existing tool risk/approval behavior is unchanged.
- Screenshot bytes and dimensions remain unchanged. Validate supported MIME against actual encoding, base64, image dimensions and bounded decoding. Limit encoded bytes to 4 MiB per image, 8 MiB per result, four images per result, 16 million pixels and 8192 pixels per edge. Oversized images are rejected rather than invisibly resized.
- Transport failures keep existing uncertain-outcome/no-replay semantics. Server `isError` is retained. A rejected image produces a tool error with the original text retained and no images, making clear that the tool already executed. Decoding runs off the async executor.
- Headless, TUI, daemon and ACP use the same runtime/tool path. Driver-specific desktop controls are outside this phase.

## Steps

1. Add protocol regression tests for standard `mimeType`/`isError` and existing snake-case fixtures; correct serde mappings without changing the negotiated MCP revision.
2. Change registry calls to retain `CallToolResult`, migrate the adapter and all direct test consumers without a second text-only compatibility API.
3. Add a private MCP result converter and image validation tests: text-only, image-only, mixed content, original dimensions, server errors, malformed data, MIME mismatch and result bounds. Enable the existing optional `image` dependency for standalone `mcp` builds.
4. Extend the existing stdio fixture with an opt-in raw result and exercise image/error results through the real transport, registry and adapter.
5. Add a coding runtime criterion proving an MCP image enters the next model request; retain the existing text tool result and call association. Check provider vision gating through existing image projection paths.
6. Document supported inline results and bounds. Run focused tests, then affected crate tests, `cargo fmt --all`, `cargo fmt --all -- --check`, `bash gates/compile.sh`, diff and lock checks. Report any unrelated baseline failures separately.

## Findings while verifying feature isolation

The existing MCP-only build depended implicitly on `tracing` and `tokio-util` from default features, while `world` unconditionally re-exported `tools::bash::LocalShell` even with tools disabled. Enable the two already-used optional dependencies explicitly for `mcp`, and gate that re-export on its owning `tools` feature. These are compile-boundary fixes needed to validate MCP independently; they add no new packages or runtime ownership.

`cargo fmt --all` also formats two pre-existing codeintel files from the baseline. Those changes are formatting only and must be kept separate from the functional commit.

## Verification

- The new standard wire-field test first failed on the baseline with `missing field mime_type`, then passed after the DTO correction.
- `cargo nextest run -p atomcode-capabilities --features mcp`: 1512 passed, 1 skipped.
- `cargo nextest run -p atomcode-coding --test runtime_criteria`: 123 passed, including the new stdio screenshot-to-next-model-request criterion.
- Final MCP-only library run (`--no-default-features --features mcp`, result/type filters): 15 passed, including all supported formats, original bytes, corrupt input, MIME mismatch and byte/pixel/count limits.
- Workspace/feature compile gate, its negative controls, rustfmt check, diff check and forbidden lock-package counts passed.
- No real desktop executor or paid model API was used. The runtime criterion observes actual model-request inputs through the existing recording provider.
