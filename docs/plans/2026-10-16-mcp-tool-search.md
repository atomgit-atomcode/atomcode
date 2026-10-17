# MCP Tool Search Implementation Plan

**Goal:** Keep large MCP catalogs out of every provider request while retaining normal named-tool execution on all model protocols.

**Architecture:** CodingRuntime remains the sole MCP scope owner. Its existing mcp-host row mounts an ordinary mcp_search tool and an AgentRequest projection. The full admitted catalog remains in ToolsSvc for native execution; only the outgoing provider tools are selected. Search returns bounded names/descriptions, and the next request reads the latest successful, paired search result from that agent's messages to expose the selected schemas. No second session state, kernel API, wire extension or persistence format is introduced.

**Tech Stack:** Rust, existing harness waterfall and tool catalog, serde JSON/TOML.

Baseline: release/v5.2.4, 84690d30c; clean worktree. Checked current registration, publication/reconnect/withdrawal, ToolBox policy/switches, native execution, current-agent realms, provider requests and recent Git history. ADR 0002 predates current harness execution: ToolsSvc is already read on every request; do not modify the retired turn-snapshot assembly.

## Scope and failure semantics

- Add [tools.mcp] tool_search auto/on/off (default auto), expose through the existing safe settings catalog, and forward through CodingRuntimeConfig/CodingAgentConfig to the mcp-host row.
- Auto uses a conservative JSON bytes/4 estimate against 10% of the current model's context window (128k fallback if unknown). Small catalogs keep eager behavior. Explicit off preserves full schemas.
- Search only the caller's admitted, enabled MCP tools. Empty queries browse with bounded pagination. Match names and descriptions deterministically. Cap selection at five and at the same schema byte budget. Return short descriptions and explicit oversized-schema status, never silently truncate schemas.
- Selected schemas replace the previous selection instead of accumulating. Only paired, successful mcp_search tool results can select names, and those names must still exist in the current outgoing catalog. Compaction/removal of the search result makes tools searchable again. Failures do not alter the previous successful selection.
- Search does not execute MCP tools or touch trust, approvals, risk metadata, plan mode or permissions. Catalog denies, per-session switches, child-agent restrictions and generation withdrawal remain authoritative. If the search tool is excluded/disabled, preserve eager behavior rather than strand tools without discovery.
- No MCP connection or credential data enters row config. New tool and listener leave with mcp-host. Reload/model/cd/resume all use the same runtime mount; no driver copies.

## Tasks

1. Add config types, settings entry, forwarding and config self-description. Test defaults, invalid modes and save/edit roundtrip.
2. Add coding/src/mcp_tool_search.rs; mount from mcp-host using existing contribution helpers. Unit-test ranking, bounded schemas/results, late catalog changes, paired-result validation, latest-selection replacement, compaction and no-search fallback.
3. Add a harness/provider regression with 34 large synthetic MCP schemas: first wire omits schemas, search enables only selected tools on the next request, native tool execution still works. Verify policy exclusion, switches and child realms; ensure no state bypasses reload/unload.
4. Update docs/mcp.md with settings and limitations. Run affected config/coding tests, existing MCP hosted-storage/runtime tests, compile gate, rustfmt and diff checks. Preserve Cargo.lock's local skip-worktree flag. Do not commit/push until requested.

## Validation and delivery (2026-10-17)

Implemented the config/setting, shared runtime forwarding (including daemon's
explicit config constructor), mcp-host contribution, request projection and docs.
Selection lives only in paired conversation facts; no mutable activation registry
was introduced. Auto mode keeps the small search tool available with eager
catalogs too, so retrying an already-filtered request remains idempotent.

- `cargo test -p atomcode-config`: 320 passed.
- Coding library + mcp_hosted_storage/runtime_criteria/tool_policy/mount_wiring/
  cache_prefix/prompt_fragments/full_assembly: 634 passed, 8 existing ignored.
- A real stdio MCP fixture exposes 34 schemas of roughly 12KB each: the first
  provider request contains none of them, a successful search exposes exactly one
  complete schema next round, its original alias executes successfully, and the
  search result stays under 2KB. Explicit off after reprepare restores all 34.
- Current-agent realm regression verifies search cannot see parent-only,
  policy-excluded, switched-off or withdrawn tools.
- Final `bash gates/compile.sh`, rustfmt check and diff whitespace check passed.
  Earlier gate runs caught transient errors in concurrent TUI/i18n work; those
  files changed independently and the final gate was rerun after their fixes.
- nextest is not installed here, so verification used cargo test. No live
  rea-agents / GLM / Qwen endpoint or Windows process-transport test was run.
- Parallel TUI/CLI/i18n and shared config/settings edits remain in the worktree;
  do not stage these files wholesale if only committing MCP work. No commit or
  push was made for this feature.

## Pre-commit audit (2026-10-17)

Reviewed schema budget/selection, paired-result parsing, caller realm resolution,
policy/switch withdrawal, original-tool approval/execution, config propagation,
reconnect/reprepare and contribution cleanup. No blocking findings. Existing
954 passing tests (8 pre-existing ignored), compile gate and fresh rustfmt check
cover the implementation; the fetched upstream commits change documentation only.
Only MCP-specific hunks of shared config/settings files belong to this commit.
Known limits remain the documented byte estimate, oversized single schemas and
lack of live rea-agents/model-endpoint verification.
